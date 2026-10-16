use std::io::Read;

use crate::api::schema::{
    AgentMessageListParams, AgentMessageSendParams, AgentMessageStatus, AgentMessageTarget,
    AgentPromptWaitOptions, Method, Request, RequestCaller,
};

const USAGE: &str = "\
usage: herdr agent message <command>

commands:
  send <target> (<text> | --file PATH | -) [--subject TEXT] [--reply-to MSG_ID] [--queue]
       [--wait] [--until STATUS]... [--timeout MS]
  reply <msg_id> (<text> | --file PATH | -) [send options]
  list [--pane PANE_ID] [--from PANE_ID] [--unread] [--thread MSG_ID] [--limit N]
  show <msg_id> [--mark-read]
  read <msg_id>
  last

Targets are pane ids, agent names or agent session ids. herdr fills in the sender from
HERDR_PANE_ID; `list` and `last` default to the inbox of that pane.";

pub(super) fn run(args: &[String]) -> std::io::Result<i32> {
    let Some(command) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    let rest = &args[1..];
    match command {
        "send" => send(rest, None),
        "reply" => {
            let Some(parent) = rest.first() else {
                eprintln!("usage: herdr agent message reply <msg_id> <text>");
                return Ok(2);
            };
            send(&rest[1..], Some(parent.clone()))
        }
        "list" => list(rest),
        "show" => show(rest),
        "read" => mark_read(rest),
        "last" => last(rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(0)
        }
        other => {
            eprintln!("unknown agent message command: {other}\n\n{USAGE}");
            Ok(2)
        }
    }
}

pub(super) fn request_caller() -> RequestCaller {
    RequestCaller {
        pane_id: std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .map(|value| super::normalize_pane_id(&value)),
        cwd: std::env::current_dir()
            .ok()
            .map(|cwd| cwd.display().to_string()),
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SendArgs {
    target: Option<String>,
    text: Option<String>,
    file: Option<String>,
    stdin: bool,
    subject: Option<String>,
    reply_to: Option<String>,
    queue: bool,
    wait: bool,
    until: Vec<crate::api::schema::AgentStatus>,
    timeout_ms: Option<u64>,
}

fn parse_send_args(args: &[String], reply_to: Option<String>) -> Result<SendArgs, String> {
    let mut parsed = SendArgs {
        reply_to: reply_to.clone(),
        ..SendArgs::default()
    };
    let mut positionals = Vec::new();
    let mut index = 0;
    let value = |index: usize, flag: &str| {
        args.get(index + 1)
            .cloned()
            .ok_or_else(|| format!("missing value for {flag}"))
    };
    while index < args.len() {
        match args[index].as_str() {
            "--file" => {
                parsed.file = Some(value(index, "--file")?);
                index += 2;
            }
            "--subject" => {
                parsed.subject = Some(value(index, "--subject")?);
                index += 2;
            }
            "--reply-to" if reply_to.is_none() => {
                parsed.reply_to = Some(value(index, "--reply-to")?);
                index += 2;
            }
            "--queue" => {
                parsed.queue = true;
                index += 1;
            }
            "--wait" => {
                parsed.wait = true;
                index += 1;
            }
            "--until" => {
                let status = super::parse_agent_status(&value(index, "--until")?)
                    .map_err(|err| err.to_string())?;
                parsed.until.push(status);
                index += 2;
            }
            "--timeout" => {
                let raw = value(index, "--timeout")?;
                parsed.timeout_ms = Some(
                    raw.parse()
                        .map_err(|_| format!("invalid --timeout value: {raw}"))?,
                );
                index += 2;
            }
            "-" => {
                parsed.stdin = true;
                index += 1;
            }
            option if option.starts_with("--") => return Err(format!("unknown option: {option}")),
            positional => {
                positionals.push(positional.to_string());
                index += 1;
            }
        }
    }
    let body_sources = usize::from(parsed.file.is_some()) + usize::from(parsed.stdin);
    let expected_positionals = usize::from(reply_to.is_none()) + usize::from(body_sources == 0);
    if body_sources > 1 {
        return Err("use only one of <text>, --file and -".into());
    }
    if positionals.len() > expected_positionals {
        return Err(format!(
            "unexpected argument: {}",
            positionals[expected_positionals]
        ));
    }
    if positionals.len() < expected_positionals {
        return Err(if reply_to.is_none() && positionals.is_empty() {
            "agent message send requires a target".into()
        } else {
            "agent message requires text, --file PATH or -".into()
        });
    }
    let mut positionals = positionals.into_iter();
    if reply_to.is_none() {
        parsed.target = positionals.next();
    }
    if body_sources == 0 {
        parsed.text = positionals.next();
    }
    if !parsed.until.is_empty() && !parsed.wait {
        return Err("--until requires --wait".into());
    }
    if parsed.timeout_ms.is_some() && !parsed.wait {
        return Err("--timeout requires --wait".into());
    }
    Ok(parsed)
}

fn send(args: &[String], reply_to: Option<String>) -> std::io::Result<i32> {
    let parsed = match parse_send_args(args, reply_to) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    let text = if let Some(path) = parsed.file.as_deref() {
        match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                eprintln!("failed to read {path}: {err}");
                return Ok(1);
            }
        }
    } else if parsed.stdin {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        text
    } else {
        parsed.text.clone().unwrap_or_default()
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:message:send".into(),
        method: Method::AgentMessageSend(AgentMessageSendParams {
            target: parsed.target,
            text,
            reply_to: parsed.reply_to,
            subject: parsed.subject,
            queue: parsed.queue,
            wait: parsed.wait.then_some(AgentPromptWaitOptions {
                until: parsed.until,
                timeout_ms: parsed.timeout_ms,
                submission_deadline: None,
            }),
            caller: Some(request_caller()),
        }),
    })?)
}

fn parse_list_args(args: &[String]) -> Result<AgentMessageListParams, String> {
    let mut params = AgentMessageListParams::default();
    let mut index = 0;
    let value = |index: usize, flag: &str| {
        args.get(index + 1)
            .cloned()
            .ok_or_else(|| format!("missing value for {flag}"))
    };
    while index < args.len() {
        match args[index].as_str() {
            "--pane" => {
                params.pane = Some(super::normalize_pane_id(&value(index, "--pane")?));
                index += 2;
            }
            "--from" => {
                params.from = Some(super::normalize_pane_id(&value(index, "--from")?));
                index += 2;
            }
            "--thread" => {
                params.thread = Some(value(index, "--thread")?);
                index += 2;
            }
            "--limit" => {
                let raw = value(index, "--limit")?;
                params.limit = Some(
                    raw.parse()
                        .map_err(|_| format!("invalid --limit value: {raw}"))?,
                );
                index += 2;
            }
            "--unread" => {
                params.unread = true;
                index += 1;
            }
            // Output is always JSON; accepted for scripts that ask for it explicitly.
            "--json" => index += 1,
            other => return Err(format!("unknown option: {other}")),
        }
    }
    Ok(params)
}

fn list(args: &[String]) -> std::io::Result<i32> {
    let mut params = match parse_list_args(args) {
        Ok(params) => params,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    params.caller = Some(request_caller());
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:message:list".into(),
        method: Method::AgentMessageList(params),
    })?)
}

fn show(args: &[String]) -> std::io::Result<i32> {
    let (id, mark_read) = match args {
        [id] => (id, false),
        [id, flag] | [flag, id] if flag == "--mark-read" => (id, true),
        _ => {
            eprintln!("usage: herdr agent message show <msg_id> [--mark-read]");
            return Ok(2);
        }
    };
    let method = if mark_read {
        Method::AgentMessageMarkRead(AgentMessageTarget { id: id.clone() })
    } else {
        Method::AgentMessageGet(AgentMessageTarget { id: id.clone() })
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:message:show".into(),
        method,
    })?)
}

fn mark_read(args: &[String]) -> std::io::Result<i32> {
    let [id] = args else {
        eprintln!("usage: herdr agent message read <msg_id>");
        return Ok(2);
    };
    super::print_response(&super::send_request(&Request {
        id: "cli:agent:message:read".into(),
        method: Method::AgentMessageMarkRead(AgentMessageTarget { id: id.clone() }),
    })?)
}

fn last(args: &[String]) -> std::io::Result<i32> {
    let mut params = match parse_list_args(args) {
        Ok(params) => params,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    let caller = request_caller();
    if params.pane.is_none() && caller.pane_id.is_none() {
        eprintln!("agent message last needs HERDR_PANE_ID or --pane");
        return Ok(2);
    }
    params.caller = Some(caller);
    let response = super::send_request(&Request {
        id: "cli:agent:message:last".into(),
        method: Method::AgentMessageList(params),
    })?;
    if response.get("error").is_some() {
        return super::print_response(&response);
    }
    let delivered = [AgentMessageStatus::Delivered, AgentMessageStatus::Read]
        .map(|status| serde_json::to_value(status).unwrap_or_default());
    let message = response["result"]["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .rev()
                .find(|message| delivered.contains(&message["status"]))
        })
        .cloned();
    let Some(message) = message else {
        eprintln!(
            "{}",
            serde_json::json!({
                "id": response["id"],
                "error": {"code": "message_not_found", "message": "no message delivered to this pane"},
            })
        );
        return Ok(1);
    };
    super::print_response(&serde_json::json!({
        "id": response["id"],
        "result": {"type": "agent_message_info", "message": message},
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn send_takes_target_and_text() {
        let parsed = parse_send_args(&args(&["w1:p2", "hi", "--subject", "ci"]), None).unwrap();
        assert_eq!(parsed.target.as_deref(), Some("w1:p2"));
        assert_eq!(parsed.text.as_deref(), Some("hi"));
        assert_eq!(parsed.subject.as_deref(), Some("ci"));
    }

    #[test]
    fn send_reads_body_from_file_or_stdin_instead_of_text() {
        let from_file = parse_send_args(&args(&["w1:p2", "--file", "body.md"]), None).unwrap();
        assert_eq!(from_file.file.as_deref(), Some("body.md"));
        assert_eq!(from_file.text, None);
        let from_stdin = parse_send_args(&args(&["w1:p2", "-"]), None).unwrap();
        assert!(from_stdin.stdin);
        assert!(parse_send_args(&args(&["w1:p2", "hi", "-"]), None).is_err());
    }

    #[test]
    fn reply_takes_only_text_and_keeps_parent() {
        let parsed = parse_send_args(&args(&["thanks"]), Some("msg_A".into())).unwrap();
        assert_eq!(parsed.target, None);
        assert_eq!(parsed.reply_to.as_deref(), Some("msg_A"));
        assert_eq!(parsed.text.as_deref(), Some("thanks"));
    }

    #[test]
    fn send_requires_target_and_wait_for_wait_options() {
        assert!(parse_send_args(&args(&[]), None).is_err());
        assert!(parse_send_args(&args(&["w1:p2"]), None).is_err());
        assert!(parse_send_args(&args(&["w1:p2", "hi", "--until", "idle"]), None).is_err());
        assert!(parse_send_args(&args(&["w1:p2", "hi", "--timeout", "5"]), None).is_err());
    }

    #[test]
    fn list_parses_filters() {
        let params = parse_list_args(&args(&[
            "--from", "w1:p1", "--unread", "--thread", "msg_A", "--json",
        ]))
        .unwrap();
        assert_eq!(params.from.as_deref(), Some("w1:p1"));
        assert!(params.unread);
        assert_eq!(params.thread.as_deref(), Some("msg_A"));
    }
}
