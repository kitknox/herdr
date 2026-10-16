use super::harness::*;

fn run_cli_from_pane(
    socket_path: &Path,
    pane_id: Option<&str>,
    args: &[&str],
) -> serde_json::Value {
    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command.args(args);
    command.env("HERDR_SOCKET_PATH", socket_path);
    match pane_id {
        Some(pane_id) => command.env("HERDR_PANE_ID", pane_id),
        None => command.env_remove("HERDR_PANE_ID"),
    };
    parse_cli_json_output(args, command.output().unwrap())
}

fn prompt_log(base: &Path, agent: &str) -> String {
    fs::read_to_string(base.join(format!("prompts-{agent}"))).unwrap_or_default()
}

fn message_status(socket_path: &Path, id: &str) -> serde_json::Value {
    run_cli_from_pane(socket_path, None, &["agent", "message", "show", id])["result"]["message"]
        .clone()
}

fn wait_for_agent_status(socket_path: &Path, target: &str, status: &str) {
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
            run_cli_json(socket_path, &["agent", "get", target])["result"]["agent"]["agent_status"]
                == status
        }),
        "agent {target} never became {status}"
    );
}

#[test]
fn agent_messages_are_attributed_threaded_and_queued() {
    use std::os::unix::fs::PermissionsExt;

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let bin = base.join("bin");
    let release = base.join("release");
    fs::create_dir_all(&bin).unwrap();
    let fake_pi = bin.join("pi");
    let herdr_bin = env!("CARGO_BIN_EXE_herdr");
    fs::write(
        &fake_pi,
        format!(
            "#!/bin/sh\n\
             export HERDR_AGENT=pi\n\
             log='{base}/prompts-'\"$1\"\n\
             report() {{ '{herdr_bin}' pane report-agent \"$HERDR_PANE_ID\" --source custom:fake-pi --agent pi --state \"$1\" >/dev/null; }}\n\
             report idle\n\
             while IFS= read -r prompt; do\n\
               printf '%s\\n' \"$prompt\" >> \"$log\"\n\
               report working\n\
               if [ \"$prompt\" = hold ]; then\n\
                 while [ ! -f '{release}' ]; do /bin/sleep 0.05; done\n\
                 /bin/rm -f '{release}'\n\
               fi\n\
               report idle\n\
             done\n",
            base = base.display(),
            release = release.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();

    let herdr = spawn_herdr_with_path(&config_home, &runtime_dir, &socket_path, Some(&bin));
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let alpha_pane = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = run_cli_json(
        &socket_path,
        &["pane", "split", &alpha_pane, "--direction", "right"],
    );
    let beta_pane = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    for (name, pane) in [("alpha", &alpha_pane), ("beta", &beta_pane)] {
        let started = run_cli_json(
            &socket_path,
            &[
                "agent",
                "start",
                name,
                "--kind",
                "pi",
                "--pane",
                pane,
                "--timeout",
                "8000",
                "--",
                name,
            ],
        );
        assert_eq!(started["result"]["type"], "agent_started");
    }

    let sent = run_cli_from_pane(
        &socket_path,
        Some(&alpha_pane),
        &["agent", "message", "send", "beta", "please review"],
    );
    let message = &sent["result"]["message"];
    let message_id = message["id"].as_str().unwrap().to_string();
    assert_eq!(message["status"], "delivered");
    assert_eq!(message["sender"]["kind"], "pane");
    assert_eq!(message["sender"]["pane_id"], alpha_pane.as_str());
    assert_eq!(message["target"]["pane_id"], beta_pane.as_str());
    let header = format!("[herdr message {message_id} from {alpha_pane} (pi, ");
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
            let log = prompt_log(&base, "beta");
            log.lines()
                .any(|line| line.starts_with(&header) && line.ends_with(']'))
                && log.contains("please review")
        }),
        "beta never received the header: {:?}",
        prompt_log(&base, "beta")
    );

    let last = run_cli_from_pane(
        &socket_path,
        Some(&beta_pane),
        &["agent", "message", "last"],
    );
    assert_eq!(last["result"]["message"]["id"], message_id.as_str());
    assert_eq!(
        last["result"]["message"]["sender"]["pane_id"],
        alpha_pane.as_str()
    );

    wait_for_agent_status(&socket_path, "beta", "idle");
    let reply = run_cli_from_pane(
        &socket_path,
        Some(&beta_pane),
        &["agent", "message", "reply", &message_id, "looks good"],
    );
    let reply_id = reply["result"]["message"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        reply["result"]["message"]["target"]["pane_id"],
        alpha_pane.as_str()
    );
    let reply_header = format!("[herdr message {reply_id} from {beta_pane} (");
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
            prompt_log(&base, "alpha").lines().any(|line| {
                line.starts_with(&reply_header) && line.ends_with(&format!(" re {message_id}]"))
            })
        }),
        "alpha never received the reply header: {:?}",
        prompt_log(&base, "alpha")
    );
    let thread = run_cli_from_pane(
        &socket_path,
        Some(&alpha_pane),
        &["agent", "message", "list", "--thread", &message_id],
    );
    let thread_ids: Vec<_> = thread["result"]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(thread_ids, [message_id.clone(), reply_id]);

    let grouped = run_cli_json(&socket_path, &["agent", "list"]);
    assert_eq!(grouped["result"]["type"], "agent_list_by_workspace");
    let groups = grouped["result"]["workspaces"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    let listed_panes: Vec<_> = groups[0]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|agent| agent["pane_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed_panes, [alpha_pane.clone(), beta_pane.clone()]);
    let in_project = run_cli_json(
        &socket_path,
        &[
            "agent",
            "list",
            "--project",
            base.to_str().unwrap(),
            "--flat",
        ],
    );
    assert_eq!(in_project["result"]["agents"].as_array().unwrap().len(), 2);
    let elsewhere = base.join("bin");
    let outside = run_cli_json(
        &socket_path,
        &["agent", "list", "--workspace", elsewhere.to_str().unwrap()],
    );
    assert!(outside["result"]["workspaces"]
        .as_array()
        .unwrap()
        .is_empty());

    let from_script = run_cli_from_pane(
        &socket_path,
        None,
        &["agent", "message", "send", &alpha_pane, "from cron"],
    );
    assert_eq!(from_script["result"]["message"]["sender"]["kind"], "cli");

    wait_for_agent_status(&socket_path, "beta", "idle");
    run_cli_json(&socket_path, &["agent", "prompt", "beta", "hold"]);
    wait_for_agent_status(&socket_path, "beta", "working");
    let queued = run_cli_from_pane(
        &socket_path,
        Some(&alpha_pane),
        &[
            "agent",
            "message",
            "send",
            "beta",
            "after your turn",
            "--queue",
        ],
    );
    let queued_id = queued["result"]["message"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(queued["result"]["message"]["status"], "queued");
    assert!(!prompt_log(&base, "beta").contains("after your turn"));
    fs::write(&release, "").unwrap();
    assert!(
        wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
            message_status(&socket_path, &queued_id)["status"] == "delivered"
        }),
        "queued message was not delivered: {}",
        message_status(&socket_path, &queued_id)
    );
    assert!(wait_until(
        Duration::from_secs(5),
        Duration::from_millis(50),
        || prompt_log(&base, "beta").contains("after your turn")
    ));

    wait_for_agent_status(&socket_path, "beta", "idle");
    run_cli_json(&socket_path, &["agent", "prompt", "beta", "hold"]);
    wait_for_agent_status(&socket_path, "beta", "working");
    let orphaned = run_cli_from_pane(
        &socket_path,
        Some(&alpha_pane),
        &["agent", "message", "send", "beta", "too late", "--queue"],
    );
    let orphaned_id = orphaned["result"]["message"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    run_cli_json(&socket_path, &["pane", "close", &beta_pane]);
    let failed = message_status(&socket_path, &orphaned_id);
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["failure"], "target_closed");

    cleanup_spawned_herdr(herdr, base);
}
