use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tracing::warn;

use crate::api::schema::{AgentMessage, AgentMessageSenderKind, AgentMessageStatus};

pub(crate) const MESSAGE_RETENTION: usize = 2000;

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Agent messages for one herdr session, mirrored to an append-only JSONL log.
pub(crate) struct MessageStore {
    messages: Vec<AgentMessage>,
    path: Option<PathBuf>,
}

impl MessageStore {
    pub(crate) fn in_memory() -> Self {
        Self {
            messages: Vec::new(),
            path: None,
        }
    }

    pub(crate) fn open(path: PathBuf) -> Self {
        let (mut messages, lines) = crate::persist::agent_messages::load(&path);
        let excess = messages.len().saturating_sub(MESSAGE_RETENTION);
        messages.drain(..excess);
        if lines > messages.len() {
            if let Err(err) = crate::persist::agent_messages::rewrite(&path, &messages) {
                warn!(path = %path.display(), err = %err, "failed to compact agent message log");
            }
        }
        Self {
            messages,
            path: Some(path),
        }
    }

    pub(crate) fn messages(&self) -> &[AgentMessage] {
        &self.messages
    }

    pub(crate) fn get(&self, id: &str) -> Option<&AgentMessage> {
        self.messages.iter().find(|message| message.id == id)
    }

    pub(crate) fn insert(&mut self, message: AgentMessage) {
        self.persist(&message);
        self.messages.push(message);
        let excess = self.messages.len().saturating_sub(MESSAGE_RETENTION);
        self.messages.drain(..excess);
    }

    pub(crate) fn update(
        &mut self,
        id: &str,
        change: impl FnOnce(&mut AgentMessage),
    ) -> Option<AgentMessage> {
        let message = self.messages.iter_mut().find(|message| message.id == id)?;
        change(message);
        let message = message.clone();
        self.persist(&message);
        Some(message)
    }

    pub(crate) fn queued_ids(&self) -> Vec<String> {
        self.messages
            .iter()
            .filter(|message| message.status == AgentMessageStatus::Queued)
            .map(|message| message.id.clone())
            .collect()
    }

    /// Ids of every message in the thread that contains `id`.
    pub(crate) fn thread_ids(&self, id: &str) -> HashSet<String> {
        let Some(root) = self.thread_root(id) else {
            return HashSet::new();
        };
        let mut thread = HashSet::from([root]);
        loop {
            let before = thread.len();
            for message in &self.messages {
                if message
                    .in_reply_to
                    .as_ref()
                    .is_some_and(|parent| thread.contains(parent))
                {
                    thread.insert(message.id.clone());
                }
            }
            if thread.len() == before {
                return thread;
            }
        }
    }

    fn thread_root(&self, id: &str) -> Option<String> {
        let mut current = self.get(id)?;
        let mut seen = HashSet::new();
        while let Some(parent) = current.in_reply_to.as_deref() {
            if !seen.insert(current.id.clone()) {
                break;
            }
            match self.get(parent) {
                Some(message) => current = message,
                None => return Some(parent.to_string()),
            }
        }
        Some(current.id.clone())
    }

    fn persist(&self, message: &AgentMessage) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        if let Err(err) = crate::persist::agent_messages::append(path, message) {
            warn!(path = %path.display(), err = %err, "failed to append agent message");
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// A ULID-shaped id: 48-bit millisecond timestamp and 80 bits of entropy, Crockford base32.
pub(crate) fn new_message_id(now_ms: u64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let mut hasher = Sha256::new();
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(nanos.to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    let digest = hasher.finalize();
    let entropy = digest[..10]
        .iter()
        .fold(0u128, |acc, byte| (acc << 8) | u128::from(*byte));
    let mut id = String::from("msg_");
    push_crockford(&mut id, u128::from(now_ms & 0xFFFF_FFFF_FFFF), 10);
    push_crockford(&mut id, entropy, 16);
    id
}

fn push_crockford(out: &mut String, value: u128, chars: u32) {
    for index in (0..chars).rev() {
        let digit = ((value >> (index * 5)) & 0x1F) as usize;
        out.push(char::from(CROCKFORD[digit]));
    }
}

/// The text typed into the target: one header line, an optional subject line, then the body.
///
/// Header grammar: `[herdr message <id> from <sender> (<agent>, <cwd>) re <id>]`. `<sender>` is a
/// pane id or `cli`; the parenthesised part lists whichever of agent and cwd are known; `re <id>`
/// is present only for replies.
pub(crate) fn delivery_text(message: &AgentMessage, home: Option<&str>) -> String {
    let mut text = format!("[herdr message {} from ", message.id);
    match message.sender.kind {
        AgentMessageSenderKind::Pane => {
            text.push_str(message.sender.pane_id.as_deref().unwrap_or("unknown"))
        }
        AgentMessageSenderKind::Cli => text.push_str("cli"),
    }
    let details: Vec<String> = [
        message.sender.agent.clone(),
        message
            .sender
            .cwd
            .as_deref()
            .map(|cwd| abbreviate_home(cwd, home)),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !details.is_empty() {
        text.push_str(" (");
        text.push_str(&details.join(", "));
        text.push(')');
    }
    if let Some(parent) = message.in_reply_to.as_deref() {
        text.push_str(" re ");
        text.push_str(parent);
    }
    text.push_str("]\n");
    if let Some(subject) = message.subject.as_deref() {
        text.push_str("Subject: ");
        text.push_str(subject);
        text.push('\n');
    }
    text.push_str(&message.body);
    text
}

fn abbreviate_home(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return path.to_string();
    };
    match path.strip_prefix(home) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{AgentMessageRecipient, AgentMessageSender};

    fn message(id: &str, in_reply_to: Option<&str>) -> AgentMessage {
        AgentMessage {
            id: id.to_string(),
            sent_at_ms: 1,
            sender: AgentMessageSender {
                kind: AgentMessageSenderKind::Pane,
                pane_id: Some("w1:p1".into()),
                workspace_id: Some("w1".into()),
                tab_id: Some("w1:t1".into()),
                agent: Some("claude".into()),
                agent_session_id: Some("sess-a".into()),
                cwd: Some("/home/ana/projects/tropos".into()),
            },
            target: AgentMessageRecipient {
                pane_id: "w1:p2".into(),
                workspace_id: Some("w1".into()),
                tab_id: Some("w1:t1".into()),
                agent: Some("codex".into()),
                agent_session_id: None,
            },
            in_reply_to: in_reply_to.map(str::to_string),
            subject: None,
            body: "hello".into(),
            status: AgentMessageStatus::Delivered,
            failure: None,
            delivered_at_ms: Some(2),
            read_at_ms: None,
        }
    }

    fn temp_log(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "herdr-agent-messages-{name}-{}-{}",
                std::process::id(),
                now_ms()
            ))
            .join("agent-messages.jsonl")
    }

    #[test]
    fn header_names_sender_and_parent() {
        let mut reply = message("msg_B", Some("msg_A"));
        reply.subject = Some("review".into());
        assert_eq!(
            delivery_text(&reply, Some("/home/ana")),
            "[herdr message msg_B from w1:p1 (claude, ~/projects/tropos) re msg_A]\nSubject: review\nhello"
        );
    }

    #[test]
    fn header_for_cli_sender_without_agent() {
        let mut sent = message("msg_A", None);
        sent.sender = AgentMessageSender {
            kind: AgentMessageSenderKind::Cli,
            pane_id: None,
            workspace_id: None,
            tab_id: None,
            agent: None,
            agent_session_id: None,
            cwd: Some("/srv/build".into()),
        };
        assert_eq!(
            delivery_text(&sent, Some("/home/ana")),
            "[herdr message msg_A from cli (/srv/build)]\nhello"
        );
    }

    #[test]
    fn home_prefix_only_abbreviates_whole_components() {
        assert_eq!(
            abbreviate_home("/home/anabel/x", Some("/home/ana")),
            "/home/anabel/x"
        );
        assert_eq!(abbreviate_home("/home/ana", Some("/home/ana")), "~");
    }

    #[test]
    fn message_ids_are_unique_sortable_and_ulid_shaped() {
        let first = new_message_id(1_000);
        let second = new_message_id(1_000);
        let later = new_message_id(2_000);
        assert_ne!(first, second);
        assert_eq!(first.len(), "msg_".len() + 26);
        assert!(first[4..].bytes().all(|byte| CROCKFORD.contains(&byte)));
        assert!(later > first);
    }

    #[test]
    fn thread_contains_root_and_all_replies() {
        let mut store = MessageStore::in_memory();
        store.insert(message("msg_A", None));
        store.insert(message("msg_B", Some("msg_A")));
        store.insert(message("msg_C", Some("msg_B")));
        store.insert(message("msg_D", None));

        let expected: HashSet<String> = ["msg_A", "msg_B", "msg_C"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(store.thread_ids("msg_C"), expected);
        assert_eq!(store.thread_ids("msg_A"), expected);
        assert!(store.thread_ids("msg_missing").is_empty());
    }

    #[test]
    fn retention_keeps_newest_messages() {
        let mut store = MessageStore::in_memory();
        for index in 0..MESSAGE_RETENTION + 3 {
            store.insert(message(&format!("msg_{index}"), None));
        }
        assert_eq!(store.messages().len(), MESSAGE_RETENTION);
        assert_eq!(store.messages()[0].id, "msg_3");
    }

    #[test]
    fn log_replays_latest_state_and_compacts() {
        let path = temp_log("replay");
        let mut store = MessageStore::open(path.clone());
        let mut queued = message("msg_A", None);
        queued.status = AgentMessageStatus::Queued;
        store.insert(queued);
        store.insert(message("msg_B", None));
        store.update("msg_A", |message| {
            message.status = AgentMessageStatus::Delivered
        });

        let reopened = MessageStore::open(path.clone());
        let ids: Vec<_> = reopened.messages().iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["msg_A", "msg_B"]);
        assert_eq!(reopened.messages()[0].status, AgentMessageStatus::Delivered);
        let lines = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .count();
        assert_eq!(lines, 2);

        let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
    }
}
