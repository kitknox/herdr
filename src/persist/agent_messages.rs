use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use tracing::warn;

use crate::api::schema::AgentMessage;

pub fn messages_path() -> PathBuf {
    crate::session::data_dir().join("agent-messages.jsonl")
}

/// Reads the message log. Each line holds a full message; the last line for an id wins and
/// messages keep the order in which their id first appeared.
pub fn load(path: &Path) -> (Vec<AgentMessage>, usize) {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), 0),
        Err(err) => {
            warn!(path = %path.display(), err = %err, "failed to read agent message log");
            return (Vec::new(), 0);
        }
    };
    let mut messages: Vec<AgentMessage> = Vec::new();
    let mut index_by_id: HashMap<String, usize> = HashMap::new();
    let mut lines = 0;
    for line in content.lines().filter(|line| !line.trim().is_empty()) {
        lines += 1;
        let message: AgentMessage = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(err) => {
                warn!(path = %path.display(), err = %err, "skipping malformed agent message record");
                continue;
            }
        };
        match index_by_id.get(&message.id) {
            Some(&index) => messages[index] = message,
            None => {
                index_by_id.insert(message.id.clone(), messages.len());
                messages.push(message);
            }
        }
    }
    (messages, lines)
}

pub fn append(path: &Path, message: &AgentMessage) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(message)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())
}

pub fn rewrite(path: &Path, messages: &[AgentMessage]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = String::new();
    for message in messages {
        content.push_str(&serde_json::to_string(message)?);
        content.push('\n');
    }
    let tmp_path = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp_path, content)?;
    #[cfg(windows)]
    if path.exists() {
        if let Err(err) = std::fs::remove_file(path) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
    }
    if let Err(err) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }
    Ok(())
}
