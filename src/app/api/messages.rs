use crate::api::schema::{
    AgentMessage, AgentMessageListParams, AgentMessageRecipient, AgentMessageSendParams,
    AgentMessageSender, AgentMessageSenderKind, AgentMessageStatus, AgentMessageTarget,
    AgentStatus, ErrorBody, EventData, EventEnvelope, EventKind, RequestCaller, ResponseResult,
};
use crate::app::agent_messages::{delivery_text, new_message_id, now_ms};
use crate::app::App;

use super::agents::DeferredSubmission;
use super::responses::{encode_error, encode_error_body, encode_success};

pub(crate) const TARGET_CLOSED: &str = "target_closed";

/// Submission errors that leave a queued message waiting for the next idle transition.
const RETRYABLE_DELIVERY_ERRORS: &[&str] = &["agent_blocked", "agent_not_ready"];

impl App {
    pub(super) fn resolve_request_sender(
        &self,
        caller: Option<&RequestCaller>,
    ) -> AgentMessageSender {
        let caller_cwd = caller.and_then(|caller| caller.cwd.clone());
        let pane = caller
            .and_then(|caller| caller.pane_id.as_deref())
            .and_then(|pane_id| self.parse_pane_id(pane_id))
            .and_then(|(ws_idx, pane_id)| self.pane_info(ws_idx, pane_id));
        match pane {
            Some(pane) => AgentMessageSender {
                kind: AgentMessageSenderKind::Pane,
                pane_id: Some(pane.pane_id),
                workspace_id: Some(pane.workspace_id),
                tab_id: Some(pane.tab_id),
                agent: pane.agent,
                agent_session_id: pane.agent_session.map(|session| session.value),
                cwd: pane.foreground_cwd.or(pane.cwd).or(caller_cwd),
            },
            None => AgentMessageSender {
                kind: AgentMessageSenderKind::Cli,
                pane_id: None,
                workspace_id: None,
                tab_id: None,
                agent: None,
                agent_session_id: None,
                cwd: caller_cwd,
            },
        }
    }

    pub(super) fn queue_agent_message_send(
        &mut self,
        id: String,
        params: AgentMessageSendParams,
    ) -> Result<DeferredSubmission, String> {
        if params.text.is_empty() {
            return Err(encode_error(
                id,
                "empty_agent_message",
                "agent message must not be empty",
            ));
        }
        #[cfg(windows)]
        let submit_deadline = params
            .wait
            .as_ref()
            .and_then(|wait| wait.submission_deadline);
        #[cfg(not(windows))]
        let submit_deadline = None;
        let parent = match params.reply_to.as_deref() {
            Some(parent_id) => match self.agent_messages.get(parent_id) {
                Some(parent) => Some(parent.clone()),
                None => return Err(message_not_found(id, parent_id)),
            },
            None => None,
        };
        let target_candidates: Vec<String> = match (&params.target, &parent) {
            (Some(target), _) => vec![target.clone()],
            (None, Some(parent)) => [
                parent.sender.pane_id.clone(),
                parent.sender.agent_session_id.clone(),
            ]
            .into_iter()
            .flatten()
            .collect(),
            (None, None) => {
                return Err(encode_error(
                    id,
                    "invalid_request",
                    "agent message requires a target or reply_to",
                ))
            }
        };
        let Some(target_label) = target_candidates.first().cloned() else {
            return Err(encode_error(
                id,
                "agent_not_found",
                "the message being answered has no sender pane to reply to",
            ));
        };
        let mut resolution = Err(None);
        for candidate in &target_candidates {
            match self.resolve_agent_target(candidate) {
                Ok(resolved) => {
                    resolution = Ok(resolved);
                    break;
                }
                Err(err) if resolution.is_err() => resolution = Err(Some(err)),
                Err(_) => {}
            }
        }
        let resolved = match resolution {
            Ok(resolved) => resolved,
            Err(Some(err)) => return Err(encode_error_body(id, self.agent_target_error_body(err))),
            Err(None) => return Err(message_target_not_found(id, &target_label)),
        };
        let Some(target_agent) = self.agent_info(resolved.ws_idx, resolved.pane_id) else {
            return Err(message_target_not_found(id, &target_label));
        };

        let sent_at_ms = now_ms();
        let mut message = AgentMessage {
            id: new_message_id(sent_at_ms),
            sent_at_ms,
            sender: self.resolve_request_sender(params.caller.as_ref()),
            target: AgentMessageRecipient {
                pane_id: target_agent.pane_id.clone(),
                workspace_id: Some(target_agent.workspace_id.clone()),
                tab_id: Some(target_agent.tab_id.clone()),
                agent: target_agent.agent.clone(),
                agent_session_id: target_agent
                    .agent_session
                    .as_ref()
                    .map(|session| session.value.clone()),
            },
            in_reply_to: params.reply_to,
            subject: params.subject.filter(|subject| !subject.is_empty()),
            body: params.text,
            status: AgentMessageStatus::Queued,
            failure: None,
            delivered_at_ms: None,
            read_at_ms: None,
        };

        let deliver_now = !params.queue
            || matches!(
                target_agent.agent_status,
                AgentStatus::Idle | AgentStatus::Done
            );
        if !deliver_now {
            return Ok(DeferredSubmission::Done(
                self.store_queued_message(id, message),
            ));
        }

        let text = delivery_text(&message, home_dir().as_deref());
        match self.submit_agent_text(&target_label, &resolved, &text, submit_deadline) {
            Ok((agent, completion)) => {
                message.status = AgentMessageStatus::Delivered;
                message.delivered_at_ms = Some(now_ms());
                self.agent_messages.insert(message.clone());
                self.emit_message_event(EventKind::AgentMessageSent, &message);
                self.emit_message_event(EventKind::AgentMessageDelivered, &message);
                Ok(DeferredSubmission::Pending {
                    id,
                    message_id: Some(message.id.clone()),
                    result: Box::new(ResponseResult::AgentMessageSent {
                        message,
                        agent: Some(agent),
                    }),
                    completion,
                })
            }
            Err(err) if params.queue && RETRYABLE_DELIVERY_ERRORS.contains(&err.code.as_str()) => {
                Ok(DeferredSubmission::Done(
                    self.store_queued_message(id, message),
                ))
            }
            Err(err) => {
                message.status = AgentMessageStatus::Failed;
                message.failure = Some(err.code.clone());
                self.agent_messages.insert(message.clone());
                self.emit_message_event(EventKind::AgentMessageSent, &message);
                self.emit_message_event(EventKind::AgentMessageFailed, &message);
                Err(encode_error_body(
                    id,
                    ErrorBody {
                        code: err.code,
                        message: format!("{} (message {})", err.message, message.id),
                    },
                ))
            }
        }
    }

    fn store_queued_message(&mut self, id: String, message: AgentMessage) -> String {
        self.agent_messages.insert(message.clone());
        self.emit_message_event(EventKind::AgentMessageSent, &message);
        encode_success(
            id,
            ResponseResult::AgentMessageSent {
                message,
                agent: None,
            },
        )
    }

    pub(crate) fn handle_agent_message_submission_failed(
        &mut self,
        message_id: &str,
        reason: String,
    ) {
        let delivered = self
            .agent_messages
            .get(message_id)
            .is_some_and(|message| message.status == AgentMessageStatus::Delivered);
        if !delivered {
            return;
        }
        if let Some(message) = self.agent_messages.update(message_id, |message| {
            message.status = AgentMessageStatus::Failed;
            message.failure = Some(reason);
            message.delivered_at_ms = None;
        }) {
            self.emit_message_event(EventKind::AgentMessageFailed, &message);
        }
    }

    pub(super) fn handle_agent_message_list(
        &mut self,
        id: String,
        params: AgentMessageListParams,
    ) -> String {
        let inbox_pane = params.pane.clone().or_else(|| {
            params
                .thread
                .is_none()
                .then(|| {
                    params
                        .caller
                        .as_ref()
                        .and_then(|caller| caller.pane_id.clone())
                })
                .flatten()
        });
        let inbox = inbox_pane.as_deref().map(|pane| self.pane_identity(pane));
        let from = params
            .from
            .as_deref()
            .map(|pane| self.pane_identity(pane).0);
        let thread = params
            .thread
            .as_deref()
            .map(|thread| self.agent_messages.thread_ids(thread));

        let mut messages: Vec<AgentMessage> = self
            .agent_messages
            .messages()
            .iter()
            .filter(|message| {
                inbox.as_ref().is_none_or(|(pane_id, session_id)| {
                    message.target.pane_id == *pane_id
                        || session_id.as_ref().is_some_and(|session_id| {
                            message.target.agent_session_id.as_ref() == Some(session_id)
                        })
                })
            })
            .filter(|message| {
                from.as_ref()
                    .is_none_or(|from| message.sender.pane_id.as_ref() == Some(from))
            })
            .filter(|message| !params.unread || message.status == AgentMessageStatus::Delivered)
            .filter(|message| {
                thread
                    .as_ref()
                    .is_none_or(|thread| thread.contains(&message.id))
            })
            .cloned()
            .collect();
        if let Some(limit) = params.limit {
            let excess = messages.len().saturating_sub(limit as usize);
            messages.drain(..excess);
        }
        encode_success(id, ResponseResult::AgentMessageList { messages })
    }

    pub(super) fn handle_agent_message_get(
        &mut self,
        id: String,
        target: AgentMessageTarget,
    ) -> String {
        match self.agent_messages.get(&target.id) {
            Some(message) => encode_success(
                id,
                ResponseResult::AgentMessageInfo {
                    message: message.clone(),
                },
            ),
            None => message_not_found(id, &target.id),
        }
    }

    pub(super) fn handle_agent_message_mark_read(
        &mut self,
        id: String,
        target: AgentMessageTarget,
    ) -> String {
        let Some(status) = self
            .agent_messages
            .get(&target.id)
            .map(|message| message.status)
        else {
            return message_not_found(id, &target.id);
        };
        let message = match status {
            AgentMessageStatus::Read => self.agent_messages.get(&target.id).cloned(),
            AgentMessageStatus::Delivered => {
                let message = self.agent_messages.update(&target.id, |message| {
                    message.status = AgentMessageStatus::Read;
                    message.read_at_ms = Some(now_ms());
                });
                if let Some(message) = message.as_ref() {
                    self.emit_message_event(EventKind::AgentMessageRead, message);
                }
                message
            }
            AgentMessageStatus::Queued | AgentMessageStatus::Failed => {
                return encode_error(
                    id,
                    "message_not_delivered",
                    format!("message {} has not been delivered", target.id),
                )
            }
        };
        match message {
            Some(message) => encode_success(id, ResponseResult::AgentMessageInfo { message }),
            None => message_not_found(id, &target.id),
        }
    }

    pub(super) fn route_queued_agent_messages(&mut self, event: &EventEnvelope) {
        match &event.data {
            EventData::PaneAgentStatusChanged {
                pane_id,
                agent_status: AgentStatus::Idle | AgentStatus::Done,
                ..
            } => {
                let pane_id = pane_id.clone();
                self.deliver_next_queued_message(&pane_id);
            }
            EventData::PaneClosed { .. }
            | EventData::PaneExited { .. }
            | EventData::TabClosed { .. }
            | EventData::WorkspaceClosed { .. } => self.fail_orphaned_queued_messages(),
            _ => {}
        }
    }

    fn deliver_next_queued_message(&mut self, pane_id: &str) {
        let Some((message, resolved)) = self
            .agent_messages
            .queued_ids()
            .into_iter()
            .filter_map(|id| self.agent_messages.get(&id).cloned())
            .find_map(|message| {
                let resolved = self.resolve_queued_target(&message)?;
                let public = self.public_pane_id(resolved.ws_idx, resolved.pane_id)?;
                (public == pane_id).then_some((message, resolved))
            })
        else {
            return;
        };
        let text = delivery_text(&message, home_dir().as_deref());
        match self.submit_agent_text(pane_id, &resolved, &text, None) {
            Ok((_, completion)) => {
                self.report_message_submission_failure(message.id.clone(), completion);
                if let Some(message) = self.agent_messages.update(&message.id, |message| {
                    message.status = AgentMessageStatus::Delivered;
                    message.delivered_at_ms = Some(now_ms());
                    message.target.pane_id = pane_id.to_string();
                }) {
                    self.emit_message_event(EventKind::AgentMessageDelivered, &message);
                }
            }
            Err(err) if RETRYABLE_DELIVERY_ERRORS.contains(&err.code.as_str()) => {}
            Err(err) => self.fail_queued_message(&message.id, err.code),
        }
    }

    fn report_message_submission_failure(
        &self,
        message_id: String,
        completion: std::sync::mpsc::Receiver<std::io::Result<()>>,
    ) {
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let reason = match completion.recv() {
                Ok(Ok(())) => return,
                Ok(Err(err)) if err.kind() == std::io::ErrorKind::TimedOut => "timeout",
                Ok(Err(_)) | Err(_) => "agent_prompt_failed",
            };
            let _ = event_tx.blocking_send(crate::events::AppEvent::AgentMessageSubmissionFailed {
                message_id,
                reason: reason.to_string(),
            });
        });
    }

    fn fail_orphaned_queued_messages(&mut self) {
        let orphaned: Vec<String> = self
            .agent_messages
            .queued_ids()
            .into_iter()
            .filter(|id| {
                self.agent_messages
                    .get(id)
                    .is_some_and(|message| self.resolve_queued_target(message).is_none())
            })
            .collect();
        for id in orphaned {
            self.fail_queued_message(&id, TARGET_CLOSED.to_string());
        }
    }

    fn fail_queued_message(&mut self, id: &str, reason: String) {
        if let Some(message) = self.agent_messages.update(id, |message| {
            message.status = AgentMessageStatus::Failed;
            message.failure = Some(reason);
        }) {
            self.emit_message_event(EventKind::AgentMessageFailed, &message);
        }
    }

    fn resolve_queued_target(
        &self,
        message: &AgentMessage,
    ) -> Option<crate::app::terminal_targets::TerminalTarget> {
        [
            Some(message.target.pane_id.as_str()),
            message.target.agent_session_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .find_map(|target| self.resolve_agent_target(target).ok())
    }

    /// The canonical public pane id and current agent session id for a pane reference, or the
    /// reference itself when it no longer resolves.
    fn pane_identity(&self, pane: &str) -> (String, Option<String>) {
        self.parse_pane_id(pane)
            .and_then(|(ws_idx, pane_id)| self.pane_info(ws_idx, pane_id))
            .map(|info| {
                (
                    info.pane_id,
                    info.agent_session.map(|session| session.value),
                )
            })
            .unwrap_or_else(|| (pane.to_string(), None))
    }

    fn emit_message_event(&mut self, event: EventKind, message: &AgentMessage) {
        let message = message.clone();
        let data = match event {
            EventKind::AgentMessageSent => EventData::AgentMessageSent { message },
            EventKind::AgentMessageDelivered => EventData::AgentMessageDelivered { message },
            EventKind::AgentMessageFailed => EventData::AgentMessageFailed { message },
            EventKind::AgentMessageRead => EventData::AgentMessageRead { message },
            _ => return,
        };
        self.emit_event(EventEnvelope { event, data });
    }
}

fn home_dir() -> Option<String> {
    std::env::var("HOME")
        .ok()
        .or_else(|| std::env::var("USERPROFILE").ok())
}

fn message_not_found(id: String, message_id: &str) -> String {
    encode_error(
        id,
        "message_not_found",
        format!("message {message_id} not found"),
    )
}

fn message_target_not_found(id: String, target: &str) -> String {
    encode_error(
        id,
        "agent_not_found",
        format!("agent target {target} not found"),
    )
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::api::schema::{
        AgentPromptParams, ErrorResponse, Method, PaneTarget, Request, SuccessResponse,
    };
    use crate::app::Mode;
    use crate::config::Config;
    use crate::detect::{Agent, AgentState};
    use crate::workspace::Workspace;

    struct Panes {
        app: App,
        sender: String,
        target: String,
        target_pane: crate::layout::PaneId,
        target_rx: tokio::sync::mpsc::Receiver<Bytes>,
        _sender_rx: tokio::sync::mpsc::Receiver<Bytes>,
    }

    fn app_with_two_agents(target_state: AgentState) -> Panes {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state.workspaces = vec![Workspace::test_new("agents")];
        let sender_pane = app.state.workspaces[0].tabs[0].root_pane;
        let target_pane =
            app.state.workspaces[0].test_split(ratatui::layout::Direction::Horizontal);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        app.state.selected = 0;
        app.state.mode = Mode::Terminal;
        for (pane, name, agent, state) in [
            (sender_pane, "planner", Agent::Claude, AgentState::Idle),
            (target_pane, "reviewer", Agent::OpenCode, target_state),
        ] {
            let terminal_id = app.state.workspaces[0].terminal_id(pane).unwrap().clone();
            let terminal = app.state.terminals.get_mut(&terminal_id).unwrap();
            terminal.set_agent_name(name.into());
            terminal.set_detected_state(Some(agent), state);
        }
        let (sender_runtime, sender_rx) =
            crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(sender_pane, sender_runtime);
        let (target_runtime, target_rx) =
            crate::terminal::TerminalRuntime::test_with_channel(80, 24);
        app.state.insert_test_runtime(target_pane, target_runtime);
        Panes {
            sender: app.public_pane_id(0, sender_pane).unwrap(),
            target: app.public_pane_id(0, target_pane).unwrap(),
            target_pane,
            app,
            target_rx,
            _sender_rx: sender_rx,
        }
    }

    fn caller(pane: &str) -> Option<RequestCaller> {
        Some(RequestCaller {
            pane_id: Some(pane.into()),
            cwd: Some("/srv/caller".into()),
        })
    }

    fn run_deferred(app: &mut App, method: Method) -> String {
        let (respond_to, response_rx) = std::sync::mpsc::channel();
        assert!(app.handle_deferred_agent_api_request(
            Request {
                id: "req".into(),
                method,
            },
            respond_to,
        ));
        response_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("deferred agent request responds")
    }

    fn send(app: &mut App, params: AgentMessageSendParams) -> AgentMessage {
        let response = run_deferred(app, Method::AgentMessageSend(params));
        let success: SuccessResponse = serde_json::from_str(&response)
            .unwrap_or_else(|_| panic!("expected success, got {response}"));
        let ResponseResult::AgentMessageSent { message, .. } = success.result else {
            panic!("expected agent_message_sent, got {response}");
        };
        message
    }

    fn send_params(target: Option<&str>, text: &str, sender: &str) -> AgentMessageSendParams {
        AgentMessageSendParams {
            target: target.map(str::to_string),
            text: text.into(),
            reply_to: None,
            subject: None,
            queue: false,
            wait: None,
            caller: caller(sender),
        }
    }

    fn list(app: &mut App, params: AgentMessageListParams) -> Vec<AgentMessage> {
        let response = app.handle_agent_message_list("req".into(), params);
        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentMessageList { messages } = success.result else {
            panic!("expected agent_message_list");
        };
        messages
    }

    fn event_kinds(app: &App) -> Vec<EventKind> {
        app.event_hub
            .events_after(0)
            .into_iter()
            .map(|(_, event)| event.event)
            .collect()
    }

    fn typed_text(rx: &mut tokio::sync::mpsc::Receiver<Bytes>) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            match rx.try_recv() {
                Ok(bytes) => return String::from_utf8(bytes.to_vec()).unwrap(),
                Err(_) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                Err(err) => panic!("message text was not typed into the target: {err}"),
            }
        }
    }

    #[tokio::test]
    async fn send_types_header_naming_sender_and_lands_in_target_inbox() {
        let Panes {
            mut app,
            sender,
            target,
            mut target_rx,
            ..
        } = app_with_two_agents(AgentState::Idle);

        let message = send(&mut app, send_params(Some("reviewer"), "check ci", &sender));

        assert_eq!(message.status, AgentMessageStatus::Delivered);
        assert_eq!(message.sender.kind, AgentMessageSenderKind::Pane);
        assert_eq!(message.sender.pane_id.as_deref(), Some(sender.as_str()));
        assert_eq!(message.target.pane_id, target);
        let typed = typed_text(&mut target_rx);
        let header = format!("[herdr message {} from {sender} (", message.id);
        assert!(typed.starts_with(&header), "{typed}");
        assert!(typed.ends_with("]\ncheck ci"), "{typed}");
        assert_eq!(
            event_kinds(&app),
            [
                EventKind::AgentMessageSent,
                EventKind::AgentMessageDelivered
            ]
        );

        let inbox = list(
            &mut app,
            AgentMessageListParams {
                caller: caller(&target),
                ..AgentMessageListParams::default()
            },
        );
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].sender.pane_id.as_deref(), Some(sender.as_str()));
        let sender_inbox = list(
            &mut app,
            AgentMessageListParams {
                caller: caller(&sender),
                ..AgentMessageListParams::default()
            },
        );
        assert!(sender_inbox.is_empty());
    }

    #[tokio::test]
    async fn caller_outside_any_pane_is_recorded_as_cli() {
        let Panes {
            mut app,
            target,
            mut target_rx,
            ..
        } = app_with_two_agents(AgentState::Idle);

        let mut params = send_params(Some(&target), "from a script", "w9:p9");
        params.caller = Some(RequestCaller {
            pane_id: None,
            cwd: Some("/srv/scripts".into()),
        });
        let message = send(&mut app, params);

        assert_eq!(message.sender.kind, AgentMessageSenderKind::Cli);
        assert_eq!(message.sender.cwd.as_deref(), Some("/srv/scripts"));
        assert!(typed_text(&mut target_rx).starts_with(&format!(
            "[herdr message {} from cli (/srv/scripts)]",
            message.id
        )));
    }

    #[tokio::test]
    async fn reply_defaults_to_parent_sender_and_threads() {
        let Panes {
            mut app,
            sender,
            target,
            target_rx: _target_rx,
            _sender_rx,
            ..
        } = app_with_two_agents(AgentState::Idle);
        let first = send(&mut app, send_params(Some(&target), "question", &sender));

        let mut reply_params = send_params(None, "answer", &target);
        reply_params.reply_to = Some(first.id.clone());
        let reply = send(&mut app, reply_params);

        assert_eq!(reply.target.pane_id, sender);
        assert_eq!(reply.in_reply_to.as_deref(), Some(first.id.as_str()));
        let thread = list(
            &mut app,
            AgentMessageListParams {
                thread: Some(reply.id.clone()),
                caller: caller(&sender),
                ..AgentMessageListParams::default()
            },
        );
        let ids: Vec<_> = thread.iter().map(|message| message.id.clone()).collect();
        assert_eq!(ids, [first.id, reply.id]);
    }

    #[tokio::test]
    async fn queued_message_is_delivered_when_target_becomes_idle() {
        let Panes {
            mut app,
            sender,
            target,
            mut target_rx,
            ..
        } = app_with_two_agents(AgentState::Working);
        let mut params = send_params(Some(&target), "when you are free", &sender);
        params.queue = true;

        let queued = send(&mut app, params);
        assert_eq!(queued.status, AgentMessageStatus::Queued);
        assert!(target_rx.try_recv().is_err());

        app.emit_event(EventEnvelope {
            event: EventKind::PaneAgentStatusChanged,
            data: EventData::PaneAgentStatusChanged {
                pane_id: target.clone(),
                workspace_id: "w1".into(),
                agent_status: AgentStatus::Idle,
                agent: None,
                title: None,
                display_agent: None,
                state_labels: Default::default(),
            },
        });

        assert!(typed_text(&mut target_rx).ends_with("when you are free"));
        assert_eq!(
            app.agent_messages.get(&queued.id).unwrap().status,
            AgentMessageStatus::Delivered
        );
        assert!(event_kinds(&app).contains(&EventKind::AgentMessageDelivered));
    }

    #[tokio::test]
    async fn queued_message_fails_when_target_pane_closes() {
        let Panes {
            mut app,
            sender,
            target,
            target_rx: _target_rx,
            ..
        } = app_with_two_agents(AgentState::Working);
        let mut params = send_params(Some(&target), "never arrives", &sender);
        params.queue = true;
        let queued = send(&mut app, params);

        app.handle_api_request(Request {
            id: "close".into(),
            method: Method::PaneClose(PaneTarget {
                pane_id: target.clone(),
            }),
        });

        let failed = app.agent_messages.get(&queued.id).unwrap();
        assert_eq!(failed.status, AgentMessageStatus::Failed);
        assert_eq!(failed.failure.as_deref(), Some(TARGET_CLOSED));
        assert!(event_kinds(&app).contains(&EventKind::AgentMessageFailed));
    }

    #[tokio::test]
    async fn blocked_target_refuses_unqueued_message_and_records_failure() {
        let Panes {
            mut app,
            sender,
            target,
            mut target_rx,
            ..
        } = app_with_two_agents(AgentState::Blocked);

        let response = run_deferred(
            &mut app,
            Method::AgentMessageSend(send_params(Some(&target), "are you there", &sender)),
        );

        let error: ErrorResponse = serde_json::from_str(&response).unwrap();
        assert_eq!(error.error.code, "agent_blocked");
        assert!(target_rx.try_recv().is_err());
        let recorded = &app.agent_messages.messages()[0];
        assert_eq!(recorded.status, AgentMessageStatus::Failed);
        assert_eq!(recorded.failure.as_deref(), Some("agent_blocked"));
    }

    #[tokio::test]
    async fn mark_read_requires_delivery_and_emits_read() {
        let Panes {
            mut app,
            sender,
            target,
            target_rx: _target_rx,
            ..
        } = app_with_two_agents(AgentState::Idle);
        let message = send(&mut app, send_params(Some(&target), "fyi", &sender));

        let response = app.handle_agent_message_mark_read(
            "req".into(),
            AgentMessageTarget {
                id: message.id.clone(),
            },
        );

        let success: SuccessResponse = serde_json::from_str(&response).unwrap();
        let ResponseResult::AgentMessageInfo { message: read } = success.result else {
            panic!("expected agent_message_info");
        };
        assert_eq!(read.status, AgentMessageStatus::Read);
        assert!(read.read_at_ms.is_some());
        assert!(event_kinds(&app).contains(&EventKind::AgentMessageRead));
        let unread = list(
            &mut app,
            AgentMessageListParams {
                unread: true,
                caller: caller(&target),
                ..AgentMessageListParams::default()
            },
        );
        assert!(unread.is_empty());
    }

    #[tokio::test]
    async fn prompt_emits_agent_prompted_naming_sender() {
        let Panes {
            mut app,
            sender,
            target,
            target_pane,
            target_rx: _target_rx,
            ..
        } = app_with_two_agents(AgentState::Idle);

        run_deferred(
            &mut app,
            Method::AgentPrompt(AgentPromptParams {
                target: target.clone(),
                text: "plain prompt".into(),
                wait: None,
                caller: caller(&sender),
            }),
        );

        let prompted = app
            .event_hub
            .events_after(0)
            .into_iter()
            .find_map(|(_, event)| match event.data {
                EventData::AgentPrompted {
                    pane_id, sender, ..
                } => Some((pane_id, sender)),
                _ => None,
            })
            .expect("agent_prompted event");
        assert_eq!(prompted.0, app.public_pane_id(0, target_pane).unwrap());
        assert_eq!(prompted.1.pane_id.as_deref(), Some(sender.as_str()));
    }

    #[tokio::test]
    async fn agent_session_id_resolves_as_message_target() {
        let Panes {
            mut app,
            target_pane,
            ..
        } = app_with_two_agents(AgentState::Idle);
        let terminal_id = app.state.workspaces[0]
            .terminal_id(target_pane)
            .unwrap()
            .clone();
        app.state
            .terminals
            .get_mut(&terminal_id)
            .unwrap()
            .persisted_agent_session = Some(crate::agent_resume::PersistedAgentSession {
            source: "test".into(),
            agent: "opencode".into(),
            session_ref: crate::agent_resume::AgentSessionRef::id("sess-reviewer").unwrap(),
        });

        let resolved = app.resolve_agent_target("sess-reviewer").unwrap();
        assert_eq!(resolved.pane_id, target_pane);
    }
}
