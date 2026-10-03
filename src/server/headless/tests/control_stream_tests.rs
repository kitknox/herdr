use super::*;

use crate::api::control::{ControlConnectionHandle, ControlOutbound};
use crate::api::schema::{
    ControlClientInfo, ControlOpenParams, EmptyParams, Method, Request, TabChrome,
    TabClaimGeometryParams, TabSetGeometryParams, TabTarget, TerminalAttachGeometry,
    TerminalAttachMode, TerminalAttachParams, TerminalAttachTarget, TerminalDetachReason,
    TerminalInputParams, TerminalQueryAuthority,
};

/// Sends a request as a control stream would and returns the response the
/// app handed back, or `None` when the server wrote it through the stream.
fn send_control(
    server: &mut HeadlessServer,
    handle: &ControlConnectionHandle,
    method: Method,
) -> Option<serde_json::Value> {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: Request {
            id: "control-test".into(),
            method,
        },
        respond_to,
        response_write_complete: None,
        control: Some(handle.clone()),
    });
    let response = response_rx.recv().expect("control response");
    if response.is_empty() {
        return None;
    }
    Some(serde_json::from_str(&response).expect("json"))
}

fn send_plain(server: &mut HeadlessServer, method: Method) -> serde_json::Value {
    let (respond_to, response_rx) = std::sync::mpsc::channel();
    server.handle_api_request_with_shutdown_check(api::ApiRequestMessage {
        request: Request {
            id: "plain".into(),
            method,
        },
        respond_to,
        response_write_complete: None,
        control: None,
    });
    serde_json::from_str(&response_rx.recv().expect("response")).expect("json")
}

fn next_line(outbound: &std::sync::mpsc::Receiver<ControlOutbound>) -> serde_json::Value {
    match outbound.try_recv() {
        Ok(ControlOutbound::Line(line)) => serde_json::from_str(&line).expect("json line"),
        other => panic!("expected a response line, got {other:?}"),
    }
}

/// Every queued record, JSON lines decoded and raw records described by
/// their `type` so tests can match on either.
fn drain(outbound: &std::sync::mpsc::Receiver<ControlOutbound>) -> Vec<serde_json::Value> {
    outbound
        .try_iter()
        .map(|record| match record {
            ControlOutbound::Line(line) => serde_json::from_str(&line).expect("json line"),
            ControlOutbound::Snapshot { attach_id, .. } => {
                serde_json::json!({"type": "terminal.snapshot", "attach_id": attach_id})
            }
            ControlOutbound::Output { attach_id, seq, .. } => {
                serde_json::json!({"type": "terminal.output", "attach_id": attach_id, "seq": seq})
            }
            ControlOutbound::Gap { attach_id, .. } => {
                serde_json::json!({"type": "terminal.gap", "attach_id": attach_id})
            }
            ControlOutbound::Detached { attach_id, reason } => serde_json::json!({
                "type": "terminal.detached",
                "attach_id": attach_id,
                "reason": serde_json::to_value(reason).unwrap(),
            }),
            ControlOutbound::Authority {
                attach_id,
                answers_queries,
            } => serde_json::json!({
                "type": "terminal.authority",
                "attach_id": attach_id,
                "answers_queries": answers_queries,
            }),
        })
        .collect()
}

fn records_of<'a>(
    records: &'a [serde_json::Value],
    kind: &'a str,
) -> impl Iterator<Item = &'a serde_json::Value> + 'a {
    records.iter().filter(move |record| record["type"] == kind)
}

fn focused_runtime(server: &HeadlessServer) -> &crate::terminal::TerminalRuntime {
    let pane_id = server.app.state.workspaces[0]
        .focused_pane_id()
        .expect("focused pane");
    server
        .app
        .state
        .runtime_for_pane_in_workspace(&server.app.terminal_runtimes, 0, pane_id)
        .expect("test runtime")
}

fn attach_params(target: &str) -> TerminalAttachParams {
    TerminalAttachParams {
        target: target.to_owned(),
        mode: TerminalAttachMode::Raw,
        history_limit_bytes: None,
        answer_queries: TerminalQueryAuthority::Client,
        takeover: false,
        geometry: TerminalAttachGeometry::Tab,
        cols: None,
        rows: None,
        cell_width_px: 0,
        cell_height_px: 0,
    }
}

fn attach(
    server: &mut HeadlessServer,
    handle: &ControlConnectionHandle,
    outbound: &std::sync::mpsc::Receiver<ControlOutbound>,
    params: TerminalAttachParams,
) -> serde_json::Value {
    let response = send_control(server, handle, Method::TerminalAttach(params));
    assert!(
        response.is_none(),
        "a successful attach answers through the stream, got {response:?}"
    );
    next_line(outbound)
}

fn attach_id_of(attached: &serde_json::Value) -> String {
    assert_eq!(
        attached["result"]["type"], "terminal_attached",
        "attach failed: {attached}"
    );
    attached["result"]["attach_id"]
        .as_str()
        .expect("attach id")
        .to_owned()
}

fn open_control_with(
    server: &mut HeadlessServer,
    params: ControlOpenParams,
) -> (
    ControlConnectionHandle,
    std::sync::mpsc::Receiver<ControlOutbound>,
    u64,
) {
    let (outbound_tx, outbound_rx) = std::sync::mpsc::channel();
    let handle = ControlConnectionHandle::new(outbound_tx);
    let opened = send_control(server, &handle, Method::ControlOpen(params)).expect("open response");
    assert_eq!(opened["result"]["type"], "control_opened");
    let connection_id = opened["result"]["connection_id"]
        .as_u64()
        .expect("connection id");
    (handle, outbound_rx, connection_id)
}

/// A protocol 1 stream: no client identity, the original contract.
fn open_control(
    server: &mut HeadlessServer,
) -> (
    ControlConnectionHandle,
    std::sync::mpsc::Receiver<ControlOutbound>,
) {
    let (handle, outbound_rx, _) = open_control_with(server, ControlOpenParams::default());
    (handle, outbound_rx)
}

fn open_control_v2(
    server: &mut HeadlessServer,
    name: &str,
) -> (
    ControlConnectionHandle,
    std::sync::mpsc::Receiver<ControlOutbound>,
    u64,
) {
    open_control_with(
        server,
        ControlOpenParams {
            client: Some(ControlClientInfo {
                name: name.into(),
                version: "test".into(),
                protocol: 2,
            }),
        },
    )
}

fn geometry_params(tab_id: &str, cols: u16, rows: u16, claim: bool) -> TabSetGeometryParams {
    TabSetGeometryParams {
        tab_id: tab_id.to_owned(),
        cols,
        rows,
        cell_width_px: 8,
        cell_height_px: 16,
        chrome: TabChrome::None,
        claim,
    }
}

fn set_tab_geometry(
    server: &mut HeadlessServer,
    handle: &ControlConnectionHandle,
    tab_id: &str,
    cols: u16,
    rows: u16,
) {
    let resized = send_control(
        server,
        handle,
        Method::TabSetGeometry(geometry_params(tab_id, cols, rows, true)),
    )
    .expect("geometry response");
    assert_eq!(
        resized["result"]["type"], "ok",
        "geometry failed: {resized}"
    );
}

fn store_tab_geometry(
    server: &mut HeadlessServer,
    handle: &ControlConnectionHandle,
    tab_id: &str,
    cols: u16,
    rows: u16,
) {
    let stored = send_control(
        server,
        handle,
        Method::TabSetGeometry(geometry_params(tab_id, cols, rows, false)),
    )
    .expect("geometry response");
    assert_eq!(stored["result"]["type"], "ok", "store failed: {stored}");
}

fn claim_tab_geometry(
    server: &mut HeadlessServer,
    handle: &ControlConnectionHandle,
    params: TabClaimGeometryParams,
) -> serde_json::Value {
    send_control(server, handle, Method::TabClaimGeometry(params)).expect("claim response")
}

fn close_control(server: &mut HeadlessServer, handle: &ControlConnectionHandle) {
    let closed = send_control(server, handle, Method::ControlClose(Default::default()))
        .expect("close response");
    assert_eq!(closed["result"]["type"], "ok");
}

fn focused_ids(server: &HeadlessServer) -> (String, String) {
    let snapshot = server.app.session_snapshot();
    (
        snapshot.focused_pane_id.clone().expect("focused pane"),
        snapshot.focused_tab_id.clone().expect("focused tab"),
    )
}

#[tokio::test]
async fn control_stream_attaches_streams_and_releases_on_close() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"hello\r\n");
    let (pane_id, tab_id) = focused_ids(&server);
    let (outbound_tx, outbound_rx) = std::sync::mpsc::channel();
    let handle = ControlConnectionHandle::new(outbound_tx);

    let opened = send_control(
        &mut server,
        &handle,
        Method::ControlOpen(Default::default()),
    )
    .expect("open response");
    assert_eq!(opened["result"]["type"], "control_opened");
    assert_eq!(opened["result"]["control_protocol"], 1);
    let connection_id = opened["result"]["connection_id"]
        .as_u64()
        .expect("connection id");
    assert_eq!(handle.id(), connection_id);
    assert!(server.control_connections.contains_key(&connection_id));

    let attached = attach(&mut server, &handle, &outbound_rx, attach_params(&pane_id));
    let attach_id = attach_id_of(&attached);
    let terminal_id = attached["result"]["terminal_id"]
        .as_str()
        .expect("terminal id")
        .to_owned();
    assert_eq!(attached["result"]["pane_id"], pane_id);
    assert!(
        server.terminal_attach_owners.is_empty(),
        "control attaches are not direct owners"
    );
    assert!(server.terminal_has_control_attaches(&terminal_id));

    match outbound_rx.try_recv() {
        Ok(ControlOutbound::Snapshot {
            attach_id: snapshot_attach,
            snapshot,
        }) => {
            assert_eq!(snapshot_attach, attach_id);
            assert_eq!(snapshot.seq, 0);
            assert!(snapshot
                .primary
                .as_deref()
                .is_some_and(|primary| primary.contains("hello")));
            assert_eq!(snapshot.state.cols, 80);
            assert_eq!(snapshot.state.rows, 24);
        }
        other => panic!("expected the attach snapshot after the response, got {other:?}"),
    }
    assert!(
        outbound_rx.try_recv().is_err(),
        "a protocol 1 stream gets no authority record"
    );
    assert!(focused_runtime(&server).suppresses_terminal_responses_for_test());

    let runtime = focused_runtime(&server);
    runtime.test_process_pty_bytes(b"world");
    match outbound_rx.try_recv() {
        Ok(ControlOutbound::Output { seq, bytes, .. }) => {
            assert_eq!(seq, 1);
            assert_eq!(&bytes[..], b"world");
        }
        other => panic!("expected raw output after the snapshot, got {other:?}"),
    }

    handle
        .input_sink(&attach_id)
        .expect("input sink")
        .try_send(bytes::Bytes::from_static(b"x"))
        .expect("input accepted");
    assert_eq!(
        input_rx.recv().await.expect("pty input"),
        bytes::Bytes::from_static(b"x")
    );

    let resized = send_control(
        &mut server,
        &handle,
        Method::TabSetGeometry(TabSetGeometryParams {
            tab_id: tab_id.clone(),
            cols: 120,
            rows: 40,
            cell_width_px: 8,
            cell_height_px: 16,
            chrome: TabChrome::None,
            claim: true,
        }),
    )
    .expect("geometry response");
    assert!(
        server.app.state.control_chromeless_tabs.contains(&tab_id),
        "chrome: none marks the tab chromeless"
    );
    assert_eq!(
        resized["result"]["type"], "ok",
        "geometry failed: {resized}"
    );
    assert_eq!(
        server.tab_geometry_controllers.get(&tab_id),
        Some(&connection_id)
    );
    let runtime = focused_runtime(&server);
    assert_ne!(
        runtime.current_size(),
        (24, 80),
        "tab geometry resized the pane"
    );
    let layout = match outbound_rx.try_recv() {
        Ok(ControlOutbound::Line(line)) => {
            serde_json::from_str::<serde_json::Value>(&line).expect("layout json")
        }
        other => panic!("expected a tab.layout record before any resize output, got {other:?}"),
    };
    assert_eq!(layout["type"], "tab.layout");
    assert_eq!(layout["layout"]["area"]["width"], 120);
    assert_eq!(layout["layout"]["area"]["height"], 40);
    assert_eq!(layout["layout"]["geometry_controller"]["kind"], "control");
    assert_eq!(
        layout["layout"]["geometry_controller"]["connection_id"],
        connection_id
    );
    assert_eq!(layout["layout"]["geometry_controller"]["chrome"], "none");
    let (rows, cols) = runtime.current_size();
    assert_eq!(layout["layout"]["panes"][0]["rect"]["width"], cols);
    assert_eq!(layout["layout"]["panes"][0]["rect"]["height"], rows);
    // Chromeless: no scrollbar gutter or border shaved off the pane.
    assert_eq!((rows, cols), (40, 120));

    let snapshot_again = send_control(
        &mut server,
        &handle,
        Method::TerminalSnapshot(TerminalAttachTarget {
            attach_id: attach_id.clone(),
        }),
    )
    .expect("snapshot response");
    assert_eq!(snapshot_again["result"]["type"], "ok");
    let records = outbound_rx.try_iter().collect::<Vec<_>>();
    assert!(
        records.iter().any(|record| matches!(
            record,
            ControlOutbound::Snapshot { snapshot, .. } if snapshot.seq == 1
        )),
        "re-snapshot carries the current sequence"
    );

    close_control(&mut server, &handle);
    assert!(server.control_connections.is_empty());
    assert!(server.terminal_attach_owners.is_empty());
    assert!(server.tab_geometry_controllers.is_empty());
    assert!(server.app.state.control_chromeless_tabs.is_empty());
    assert!(!handle.is_alive());
    let records = outbound_rx.try_iter().collect::<Vec<_>>();
    assert!(
        records.iter().any(|record| matches!(
            record,
            ControlOutbound::Detached {
                reason: TerminalDetachReason::Closed,
                ..
            }
        )),
        "closing the stream detaches every attach"
    );
}

#[tokio::test]
async fn control_open_negotiates_protocol_and_advertises_features() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");

    let (v1, _v1_rx, _) = open_control_with(&mut server, ControlOpenParams::default());
    assert_eq!(v1.protocol(), 1);

    let (outbound_tx, _outbound_rx) = std::sync::mpsc::channel();
    let eager = ControlConnectionHandle::new(outbound_tx);
    let opened = send_control(
        &mut server,
        &eager,
        Method::ControlOpen(ControlOpenParams {
            client: Some(ControlClientInfo {
                name: "future".into(),
                version: "9".into(),
                protocol: 9,
            }),
        }),
    )
    .expect("open response");
    assert_eq!(
        opened["result"]["control_protocol"], 3,
        "negotiates down to what the server supports"
    );
    assert_eq!(eager.protocol(), 3);
    assert_eq!(
        opened["result"]["capabilities"]["terminal_control_stream"],
        3
    );
    let features = opened["result"]["capabilities"]["control_features"]
        .as_array()
        .expect("features");
    for feature in ["shared_attach", "geometry_ownership", "control_list"] {
        assert!(
            features.iter().any(|value| value == feature),
            "missing {feature} in {features:?}"
        );
    }

    let (outbound_tx, _outbound_rx) = std::sync::mpsc::channel();
    let zero = ControlConnectionHandle::new(outbound_tx);
    let opened = send_control(
        &mut server,
        &zero,
        Method::ControlOpen(ControlOpenParams {
            client: Some(ControlClientInfo::default()),
        }),
    )
    .expect("open response");
    assert_eq!(
        opened["result"]["control_protocol"], 1,
        "a client that names no protocol gets the original contract"
    );
}

#[tokio::test]
async fn second_control_attach_shares_the_terminal() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, _) = focused_ids(&server);
    let (first, first_rx, _) = open_control_v2(&mut server, "a");
    let (second, second_rx, _) = open_control_v2(&mut server, "b");

    let first_attach = attach_id_of(&attach(
        &mut server,
        &first,
        &first_rx,
        attach_params(&pane_id),
    ));
    let second_attach = attach_id_of(&attach(
        &mut server,
        &second,
        &second_rx,
        attach_params(&pane_id),
    ));
    assert!(server.terminal_attach_owners.is_empty());
    assert!(
        first.input_sink(&first_attach).is_some(),
        "sharing evicts nobody"
    );
    let first_records = drain(&first_rx);
    let second_records = drain(&second_rx);
    assert_eq!(records_of(&first_records, "terminal.snapshot").count(), 1);
    assert_eq!(records_of(&second_records, "terminal.snapshot").count(), 1);
    assert!(
        records_of(&first_records, "terminal.detached")
            .next()
            .is_none(),
        "{first_records:?}"
    );

    focused_runtime(&server).test_process_pty_bytes(b"shared");
    for (rx, attach_id) in [(&first_rx, &first_attach), (&second_rx, &second_attach)] {
        let records = drain(rx);
        let output = records_of(&records, "terminal.output")
            .next()
            .expect("output fans out to every tap");
        assert_eq!(output["attach_id"], *attach_id);
        assert_eq!(output["seq"], 1, "each tap numbers its own output");
    }

    for (handle, attach_id, byte) in [
        (&first, &first_attach, b"1"),
        (&second, &second_attach, b"2"),
    ] {
        handle
            .input_sink(attach_id)
            .expect("input sink")
            .try_send(bytes::Bytes::from_static(byte))
            .expect("input accepted");
        assert_eq!(
            input_rx.recv().await.expect("pty input"),
            bytes::Bytes::from_static(byte)
        );
    }

    close_control(&mut server, &first);
    assert!(
        second.input_sink(&second_attach).is_some(),
        "one stream closing leaves the other attached"
    );
    let records = drain(&second_rx);
    assert!(records_of(&records, "terminal.detached").next().is_none());
}

#[tokio::test]
async fn takeover_evicts_v1_attaches_but_not_v2() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, _) = focused_ids(&server);
    let (v1_a, v1_a_rx) = open_control(&mut server);
    let (v2, v2_rx, _) = open_control_v2(&mut server, "b");
    let (v1_c, v1_c_rx) = open_control(&mut server);

    // Today's rootshell: protocol 1, always takeover.
    let mut takeover = attach_params(&pane_id);
    takeover.takeover = true;
    let a_attach = attach_id_of(&attach(&mut server, &v1_a, &v1_a_rx, takeover.clone()));
    let b_attach = attach_id_of(&attach(&mut server, &v2, &v2_rx, attach_params(&pane_id)));
    assert!(
        v1_a.input_sink(&a_attach).is_some(),
        "a v2 join evicts nobody"
    );

    let c_attach = attach_id_of(&attach(&mut server, &v1_c, &v1_c_rx, takeover.clone()));
    assert!(
        v1_a.input_sink(&a_attach).is_none(),
        "a v1 takeover evicts the other v1 attach"
    );
    let a_records = drain(&v1_a_rx);
    assert!(
        records_of(&a_records, "terminal.detached").any(|record| record["reason"] == "takeover")
    );
    assert!(
        v2.input_sink(&b_attach).is_some(),
        "a v1 takeover leaves the v2 attach alone"
    );
    let b_records = drain(&v2_rx);
    assert!(records_of(&b_records, "terminal.detached").next().is_none());

    // A size owner cannot share with anyone.
    let (owner, owner_rx, _) = open_control_v2(&mut server, "d");
    let mut sized = attach_params(&pane_id);
    sized.geometry = TerminalAttachGeometry::Terminal;
    sized.cols = Some(50);
    sized.rows = Some(10);
    let refused =
        send_control(&mut server, &owner, Method::TerminalAttach(sized.clone())).expect("refusal");
    assert_eq!(refused["error"]["code"], "terminal_attached");
    sized.takeover = true;
    let d_attach = attach_id_of(&attach(&mut server, &owner, &owner_rx, sized));
    assert!(v2.input_sink(&b_attach).is_none());
    assert!(v1_c.input_sink(&c_attach).is_none());
    for rx in [&v2_rx, &v1_c_rx] {
        let records = drain(rx);
        assert!(
            records_of(&records, "terminal.detached").any(|record| record["reason"] == "takeover")
        );
    }
    assert!(owner.input_sink(&d_attach).is_some());
}

#[tokio::test]
async fn tab_following_attach_refused_while_terminal_geometry_attach_exists() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, _) = focused_ids(&server);
    let (owner, owner_rx, _) = open_control_v2(&mut server, "owner");
    let (follower, follower_rx, _) = open_control_v2(&mut server, "follower");

    let mut sized = attach_params(&pane_id);
    sized.geometry = TerminalAttachGeometry::Terminal;
    sized.cols = Some(50);
    sized.rows = Some(10);
    let owner_attach = attach_id_of(&attach(&mut server, &owner, &owner_rx, sized));
    assert_eq!(focused_runtime(&server).current_size(), (10, 50));

    let refused = send_control(
        &mut server,
        &follower,
        Method::TerminalAttach(attach_params(&pane_id)),
    )
    .expect("refusal");
    assert_eq!(refused["error"]["code"], "terminal_attached");

    let mut takeover = attach_params(&pane_id);
    takeover.takeover = true;
    attach_id_of(&attach(&mut server, &follower, &follower_rx, takeover));
    assert!(
        owner.input_sink(&owner_attach).is_none(),
        "a follower's takeover evicts the size owner"
    );
    let records = drain(&owner_rx);
    assert!(records_of(&records, "terminal.detached").any(|record| record["reason"] == "takeover"));
}

#[tokio::test]
async fn terminal_geometry_attach_refused_while_followers_exist() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, _) = focused_ids(&server);
    let (follower, follower_rx, _) = open_control_v2(&mut server, "follower");
    let (owner, _owner_rx, _) = open_control_v2(&mut server, "owner");

    attach_id_of(&attach(
        &mut server,
        &follower,
        &follower_rx,
        attach_params(&pane_id),
    ));
    let mut sized = attach_params(&pane_id);
    sized.geometry = TerminalAttachGeometry::Terminal;
    let refused =
        send_control(&mut server, &owner, Method::TerminalAttach(sized)).expect("refusal");
    assert_eq!(refused["error"]["code"], "terminal_attached");
}

#[tokio::test]
async fn control_methods_need_an_open_stream() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let response = send_plain(&mut server, Method::TerminalAttach(attach_params("w1:p1")));
    assert_eq!(response["error"]["code"], "control_stream_required");
    let response = send_plain(
        &mut server,
        Method::TabClaimGeometry(TabClaimGeometryParams::default()),
    );
    assert_eq!(response["error"]["code"], "control_stream_required");
}

#[tokio::test]
async fn geometry_follows_latest_control_interaction() {
    let event_hub = api::EventHub::default();
    let mut server = test_headless_server_with_event_hub(event_hub.clone());
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, a_id) = open_control_v2(&mut server, "a");
    let (b, b_rx, b_id) = open_control_v2(&mut server, "b");
    attach_id_of(&attach(&mut server, &a, &a_rx, attach_params(&pane_id)));
    attach_id_of(&attach(&mut server, &b, &b_rx, attach_params(&pane_id)));
    let _ = drain(&a_rx);
    let _ = drain(&b_rx);

    set_tab_geometry(&mut server, &a, &tab_id, 120, 40);
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&a_id));
    assert_eq!(focused_runtime(&server).current_size(), (40, 120));

    set_tab_geometry(&mut server, &b, &tab_id, 100, 30);
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&b_id));
    assert_eq!(focused_runtime(&server).current_size(), (30, 100));
    for rx in [&a_rx, &b_rx] {
        let records = drain(rx);
        let layout = records_of(&records, "tab.layout")
            .last()
            .expect("every stream sees the layout");
        assert_eq!(
            layout["layout"]["geometry_controller"]["connection_id"],
            b_id
        );
        assert_eq!(layout["layout"]["area"]["width"], 100);
    }
    assert!(
        a.take_claim_on_input(&format!("{}-0", a_id - (1 << 40))),
        "the stream that lost the tab claims it back on input"
    );

    let claimed = claim_tab_geometry(
        &mut server,
        &a,
        TabClaimGeometryParams {
            tab_id: Some(tab_id.clone()),
            attach_id: None,
        },
    );
    assert_eq!(claimed["result"]["type"], "ok", "{claimed}");
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&a_id));
    assert_eq!(focused_runtime(&server).current_size(), (40, 120));

    let changes = event_hub
        .events_after(0)
        .into_iter()
        .filter(|(_, event)| event.event == api::schema::EventKind::TabGeometryChanged)
        .map(|(_, event)| serde_json::to_value(event).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(changes.len(), 3, "{changes:?}");
    assert_eq!(
        changes[0]["data"]["geometry_controller"]["connection_id"],
        a_id
    );
    assert_eq!(changes[0]["data"]["previous"], serde_json::Value::Null);
    assert_eq!(
        changes[1]["data"]["geometry_controller"]["connection_id"],
        b_id
    );
    assert_eq!(changes[1]["data"]["previous"]["connection_id"], a_id);
    assert_eq!(
        changes[2]["data"]["geometry_controller"]["connection_id"],
        a_id
    );
    assert_eq!(changes[2]["data"]["previous"]["connection_id"], b_id);
    assert_eq!(changes[2]["data"]["tab_id"], tab_id);

    let snapshot = server.app.session_snapshot();
    let layout = snapshot
        .layouts
        .iter()
        .find(|layout| layout.tab_id == tab_id)
        .expect("tab layout in the session snapshot");
    assert_eq!(
        layout
            .geometry_controller
            .as_ref()
            .and_then(|controller| controller.connection_id),
        Some(a_id),
        "session.snapshot layouts carry the controller too"
    );
}

#[tokio::test]
async fn set_geometry_without_claim_does_not_relayout() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, a_id) = open_control_v2(&mut server, "a");
    let (b, b_rx, _) = open_control_v2(&mut server, "b");
    attach_id_of(&attach(&mut server, &a, &a_rx, attach_params(&pane_id)));
    attach_id_of(&attach(&mut server, &b, &b_rx, attach_params(&pane_id)));
    set_tab_geometry(&mut server, &a, &tab_id, 120, 40);
    let _ = drain(&a_rx);
    let _ = drain(&b_rx);

    store_tab_geometry(&mut server, &b, &tab_id, 100, 30);
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&a_id));
    assert_eq!(focused_runtime(&server).current_size(), (40, 120));
    let records = drain(&b_rx);
    assert!(
        records_of(&records, "tab.layout").next().is_none(),
        "storing a size lays nothing out: {records:?}"
    );

    // The owner updating its own size still applies it.
    store_tab_geometry(&mut server, &a, &tab_id, 110, 35);
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&a_id));
    assert_eq!(focused_runtime(&server).current_size(), (35, 110));
}

#[tokio::test]
async fn input_claims_geometry_only_with_stored_geometry() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, a_id) = open_control_v2(&mut server, "a");
    let (b, b_rx, b_id) = open_control_v2(&mut server, "b");
    let a_attach = attach_id_of(&attach(&mut server, &a, &a_rx, attach_params(&pane_id)));
    let b_attach = attach_id_of(&attach(&mut server, &b, &b_rx, attach_params(&pane_id)));
    set_tab_geometry(&mut server, &a, &tab_id, 120, 40);
    assert!(
        !b.take_claim_on_input(&b_attach),
        "no stored size: typing cannot steal the tab"
    );

    store_tab_geometry(&mut server, &b, &tab_id, 100, 30);
    assert!(
        b.take_claim_on_input(&b_attach),
        "a stored size arms the input claim"
    );
    assert!(!b.take_claim_on_input(&b_attach), "one input, one claim");
    // The socket side dispatches this on the flagged input.
    let claimed = claim_tab_geometry(
        &mut server,
        &b,
        TabClaimGeometryParams {
            tab_id: None,
            attach_id: Some(b_attach.clone()),
        },
    );
    assert_eq!(claimed["result"]["type"], "ok", "{claimed}");
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&b_id));
    assert_eq!(focused_runtime(&server).current_size(), (30, 100));
    assert!(
        a.take_claim_on_input(&a_attach),
        "the previous owner now claims back on input"
    );
    assert!(
        !b.take_claim_on_input(&b_attach),
        "the owner has nothing to claim"
    );

    let (c, c_rx, _) = open_control_v2(&mut server, "c");
    let c_attach = attach_id_of(&attach(&mut server, &c, &c_rx, attach_params(&pane_id)));
    let missing = claim_tab_geometry(
        &mut server,
        &c,
        TabClaimGeometryParams {
            tab_id: None,
            attach_id: Some(c_attach),
        },
    );
    assert_eq!(missing["error"]["code"], "no_geometry");
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&b_id));
    let _ = a_id;
}

#[tokio::test]
async fn pane_focus_never_claims() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, a_id) = open_control_v2(&mut server, "a");
    let (b, b_rx, _) = open_control_v2(&mut server, "b");
    attach_id_of(&attach(&mut server, &a, &a_rx, attach_params(&pane_id)));
    attach_id_of(&attach(&mut server, &b, &b_rx, attach_params(&pane_id)));
    set_tab_geometry(&mut server, &a, &tab_id, 120, 40);
    store_tab_geometry(&mut server, &b, &tab_id, 100, 30);

    let focused = send_control(
        &mut server,
        &b,
        Method::TabFocus(TabTarget {
            tab_id: tab_id.clone(),
        }),
    )
    .expect("focus response");
    assert!(focused.get("error").is_none(), "{focused}");
    assert_eq!(
        server.tab_geometry_controllers.get(&tab_id),
        Some(&a_id),
        "focus from a control stream leaves geometry alone"
    );
    assert_eq!(focused_runtime(&server).current_size(), (40, 120));
}

#[tokio::test]
async fn owner_disconnect_hands_tab_to_latest_remaining_geometry() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, a_id) = open_control_v2(&mut server, "a");
    let (b, b_rx, b_id) = open_control_v2(&mut server, "b");
    let (c, c_rx, c_id) = open_control_v2(&mut server, "c");
    for (handle, rx) in [(&a, &a_rx), (&b, &b_rx), (&c, &c_rx)] {
        attach_id_of(&attach(&mut server, handle, rx, attach_params(&pane_id)));
    }
    store_tab_geometry(&mut server, &a, &tab_id, 90, 20);
    store_tab_geometry(&mut server, &c, &tab_id, 100, 30);
    set_tab_geometry(&mut server, &b, &tab_id, 120, 40);
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&b_id));

    close_control(&mut server, &b);
    assert_eq!(
        server.tab_geometry_controllers.get(&tab_id),
        Some(&c_id),
        "the most recent remaining size wins"
    );
    assert_eq!(focused_runtime(&server).current_size(), (30, 100));
    let records = drain(&a_rx);
    let layout = records_of(&records, "tab.layout")
        .last()
        .expect("survivors see the hand-off");
    assert_eq!(
        layout["layout"]["geometry_controller"]["connection_id"],
        c_id
    );

    close_control(&mut server, &c);
    assert_eq!(server.tab_geometry_controllers.get(&tab_id), Some(&a_id));
    assert_eq!(focused_runtime(&server).current_size(), (20, 90));

    close_control(&mut server, &a);
    assert!(server.tab_geometry_controllers.is_empty());
    assert!(server.control_connections.is_empty());
}

#[tokio::test]
async fn query_authority_prefers_v1_then_geometry_controller() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, _) = open_control_v2(&mut server, "a");
    let (b, b_rx, _) = open_control_v2(&mut server, "b");

    let a_attach = attach_id_of(&attach(&mut server, &a, &a_rx, attach_params(&pane_id)));
    let records = drain(&a_rx);
    let authority = records_of(&records, "terminal.authority")
        .next()
        .expect("a protocol 2 attach learns its standing after the snapshot");
    assert_eq!(authority["attach_id"], a_attach);
    assert_eq!(authority["answers_queries"], true);
    assert_eq!(
        focused_runtime(&server)
            .raw_query_authority_for_test()
            .as_deref(),
        Some(a_attach.as_str())
    );
    assert!(focused_runtime(&server).suppresses_terminal_responses_for_test());

    let b_attach = attach_id_of(&attach(&mut server, &b, &b_rx, attach_params(&pane_id)));
    let records = drain(&b_rx);
    assert!(
        records_of(&records, "terminal.authority").any(|record| record["answers_queries"] == false),
        "the oldest attach keeps answering: {records:?}"
    );

    set_tab_geometry(&mut server, &b, &tab_id, 100, 30);
    assert_eq!(
        focused_runtime(&server)
            .raw_query_authority_for_test()
            .as_deref(),
        Some(b_attach.as_str()),
        "the geometry controller answers"
    );
    let a_records = drain(&a_rx);
    assert!(records_of(&a_records, "terminal.authority")
        .any(|record| record["answers_queries"] == false));
    let b_records = drain(&b_rx);
    assert!(records_of(&b_records, "terminal.authority")
        .any(|record| record["answers_queries"] == true));

    // A protocol 1 stream is exclusive, so it joins with takeover; that
    // evicts protocol 1 followers only, leaving a and b in place.
    let (v1, v1_rx) = open_control(&mut server);
    let v1_params = TerminalAttachParams {
        takeover: true,
        ..attach_params(&pane_id)
    };
    let v1_attach = attach_id_of(&attach(&mut server, &v1, &v1_rx, v1_params));
    assert_eq!(
        focused_runtime(&server)
            .raw_query_authority_for_test()
            .as_deref(),
        Some(v1_attach.as_str()),
        "a protocol 1 client cannot be told to stop, so it answers"
    );
    let v1_records = drain(&v1_rx);
    assert!(
        records_of(&v1_records, "terminal.authority")
            .next()
            .is_none(),
        "{v1_records:?}"
    );
    let b_records = drain(&b_rx);
    assert!(records_of(&b_records, "terminal.authority")
        .any(|record| record["answers_queries"] == false));
    assert!(focused_runtime(&server).suppresses_terminal_responses_for_test());

    close_control(&mut server, &v1);
    assert_eq!(
        focused_runtime(&server)
            .raw_query_authority_for_test()
            .as_deref(),
        Some(b_attach.as_str())
    );

    let listed = send_control(&mut server, &b, Method::ControlList(Default::default()))
        .expect("list response");
    let connections = listed["result"]["connections"].as_array().expect("list");
    let answering = connections
        .iter()
        .flat_map(|connection| connection["attaches"].as_array().unwrap().iter())
        .filter(|attach| attach["answers_queries"] == true)
        .count();
    assert_eq!(answering, 1);
}

#[tokio::test]
async fn control_list_reports_clients_attaches_and_tabs() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, a_id) = open_control_v2(&mut server, "rootshell");
    let (b, _b_rx) = open_control(&mut server);
    let a_attach = attach_id_of(&attach(&mut server, &a, &a_rx, attach_params(&pane_id)));
    set_tab_geometry(&mut server, &a, &tab_id, 120, 40);
    store_tab_geometry(&mut server, &b, &tab_id, 80, 24);

    let listed = send_control(&mut server, &a, Method::ControlList(Default::default()))
        .expect("list response");
    assert_eq!(listed["result"]["type"], "control_list");
    assert_eq!(listed["result"]["self_connection_id"], a_id);
    let connections = listed["result"]["connections"].as_array().expect("list");
    assert_eq!(connections.len(), 2);
    let first = &connections[0];
    assert_eq!(first["connection_id"], a_id);
    assert_eq!(first["control_protocol"], 2);
    assert_eq!(first["client"]["name"], "rootshell");
    assert_eq!(first["attaches"][0]["attach_id"], a_attach);
    assert_eq!(first["attaches"][0]["pane_id"], pane_id);
    assert_eq!(first["attaches"][0]["geometry"], "tab");
    assert_eq!(first["attaches"][0]["answers_queries"], true);
    assert_eq!(first["tabs"][0]["tab_id"], tab_id);
    assert_eq!(first["tabs"][0]["cols"], 120);
    assert_eq!(first["tabs"][0]["controller"], true);
    let second = &connections[1];
    assert_eq!(second["control_protocol"], 1);
    assert!(second.get("client").is_none());
    assert_eq!(second["tabs"][0]["controller"], false);

    let plain = send_plain(&mut server, Method::ControlList(Default::default()));
    assert_eq!(plain["result"]["type"], "control_list");
    assert!(plain["result"].get("self_connection_id").is_none());
    assert_eq!(plain["result"]["connections"].as_array().unwrap().len(), 2);
}

/// With only control connections attached, the render loop lays herdr's
/// own view out at the headless size. A tab a control connection sized must
/// keep that size, and no `tab.layout` record may be emitted for it.
#[tokio::test]
async fn no_client_render_keeps_control_owned_tab_geometry() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"hello\r\n");
    let (_, tab_id) = focused_ids(&server);
    let (handle, outbound_rx) = open_control(&mut server);
    set_tab_geometry(&mut server, &handle, &tab_id, 120, 40);
    assert_eq!(focused_runtime(&server).current_size(), (40, 120));
    assert!(server.app.state.control_geometry_tabs.contains(&tab_id));
    let _ = outbound_rx.try_iter().count();

    assert!(server.app.state.view.pane_infos.is_empty());
    server.render_and_stream();

    assert_eq!(focused_runtime(&server).current_size(), (40, 120));
    assert!(
        outbound_rx.try_iter().all(|record| !matches!(
            record,
            ControlOutbound::Line(ref line) if line.contains("tab.layout")
        )),
        "a view recompute must not resize a control-owned tab"
    );

    close_control(&mut server, &handle);
    assert!(server.app.state.control_geometry_tabs.is_empty());
}

/// A herdr shell client resizing its window and a layout action recomputing
/// the view both resize background tabs; a control-owned one is skipped.
#[tokio::test]
async fn shell_client_and_layout_recompute_keep_control_owned_background_tab_geometry() {
    let mut server = test_headless_server();
    let mut workspace = crate::workspace::Workspace::test_new("control-owned-background");
    let first_pane = workspace.tabs[0].root_pane;
    let second_tab = workspace.test_add_tab(Some("second"));
    let second_pane = workspace.tabs[second_tab].root_pane;
    workspace.insert_test_runtime(
        first_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"FIRST_TAB"),
    );
    workspace.insert_test_runtime(
        second_pane,
        crate::terminal::TerminalRuntime::test_with_screen_bytes(80, 24, b"SECOND_TAB"),
    );
    server.app.state.workspaces = vec![workspace];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let second_tab_id = server.app.public_tab_id(0, second_tab).unwrap();
    let size_of = |server: &HeadlessServer, pane| {
        server.app.state.workspaces[0].test_runtimes[&pane].current_size()
    };

    let (handle, _outbound_rx) = open_control(&mut server);
    set_tab_geometry(&mut server, &handle, &second_tab_id, 120, 40);
    assert_eq!(size_of(&server, second_pane), (40, 120));

    let (first_control, _first_render) = connect_test_shell(&mut server, 21, 100, 30);
    let _ = first_control.recv().expect("first snapshot");
    assert_ne!(size_of(&server, first_pane), (24, 80));
    assert_eq!(size_of(&server, second_pane), (40, 120));

    crate::ui::compute_view_with_runtime_registry(
        &mut server.app.state,
        &server.app.terminal_runtimes,
        ratatui::layout::Rect::new(0, 0, 60, 20),
    );
    assert_ne!(size_of(&server, first_pane), (40, 120));
    assert_eq!(size_of(&server, second_pane), (40, 120));
}

/// A joining stream bootstraps from `session.snapshot`; its layouts must be
/// the ones `tab.layout` records carry, not the TUI's view area.
#[tokio::test]
async fn control_snapshot_layouts_match_tab_layout_records() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, a_id) = open_control_v2(&mut server, "a");
    let _a_attach = attach_id_of(&attach(&mut server, &a, &a_rx, attach_params(&pane_id)));
    drain(&a_rx);
    set_tab_geometry(&mut server, &a, &tab_id, 120, 40);
    let records = drain(&a_rx);
    let record = records_of(&records, "tab.layout")
        .filter(|record| record["layout"]["tab_id"] == tab_id)
        .last()
        .expect("tab.layout for the sized tab")
        .clone();
    assert_eq!(record["layout"]["area"]["width"], 120);
    assert_eq!(record["layout"]["area"]["height"], 40);

    let (b, _b_rx, _) = open_control_v2(&mut server, "b");
    let snapshot = send_control(&mut server, &b, Method::SessionSnapshot(EmptyParams {}))
        .expect("snapshot response");
    let layouts = snapshot["result"]["snapshot"]["layouts"]
        .as_array()
        .expect("layouts");
    let layout = layouts
        .iter()
        .find(|layout| layout["tab_id"] == tab_id)
        .expect("layout for the sized tab");
    assert_eq!(layout["area"], record["layout"]["area"]);
    assert_eq!(layout["panes"], record["layout"]["panes"]);
    assert_eq!(layout["splits"], record["layout"]["splits"]);
    assert_eq!(layout["geometry_controller"]["connection_id"], a_id);
    assert_eq!(layout, &record["layout"]);

    // Plain API callers keep the shared view area.
    let plain = send_plain(&mut server, Method::SessionSnapshot(EmptyParams {}));
    let plain_layout = plain["result"]["snapshot"]["layouts"]
        .as_array()
        .and_then(|layouts| layouts.iter().find(|layout| layout["tab_id"] == tab_id))
        .expect("plain layout");
    let view = server.app.state.view.terminal_area;
    assert_eq!(plain_layout["area"]["width"], view.width);
    assert_eq!(plain_layout["area"]["height"], view.height);
}

/// Protocol 1 keeps its one-owner contract even on a sharing server.
#[tokio::test]
async fn protocol_one_attaches_stay_exclusive() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, _) = focused_ids(&server);
    let (v1_a, v1_a_rx) = open_control(&mut server);
    let (v1_b, _v1_b_rx) = open_control(&mut server);
    let a_attach = attach_id_of(&attach(
        &mut server,
        &v1_a,
        &v1_a_rx,
        attach_params(&pane_id),
    ));

    let refused = send_control(
        &mut server,
        &v1_b,
        Method::TerminalAttach(attach_params(&pane_id)),
    )
    .expect("refusal");
    assert_eq!(
        refused["error"]["code"], "terminal_attached",
        "a second protocol 1 attach needs takeover: {refused}"
    );
    assert!(v1_a.input_sink(&a_attach).is_some());

    // A protocol 2 follower joins the v1 attach; the v1 attach keeps answering.
    let (v2, v2_rx, _) = open_control_v2(&mut server, "b");
    let b_attach = attach_id_of(&attach(&mut server, &v2, &v2_rx, attach_params(&pane_id)));
    assert!(
        v1_a.input_sink(&a_attach).is_some(),
        "a v2 join evicts nobody"
    );
    assert_eq!(
        focused_runtime(&server)
            .raw_query_authority_for_test()
            .as_deref(),
        Some(a_attach.as_str())
    );
    let records = drain(&v2_rx);
    assert!(records_of(&records, "terminal.authority")
        .any(|record| record["attach_id"] == b_attach && record["answers_queries"] == false));

    // Another protocol 1 client is still refused while any attach exists.
    let refused = send_control(
        &mut server,
        &v1_b,
        Method::TerminalAttach(attach_params(&pane_id)),
    )
    .expect("refusal");
    assert_eq!(refused["error"]["code"], "terminal_attached");

    // With takeover it evicts the v1 attach only.
    let (_v1_b, v1_b_rx) = (v1_b, _v1_b_rx);
    let mut takeover = attach_params(&pane_id);
    takeover.takeover = true;
    let _c_attach = attach_id_of(&attach(&mut server, &_v1_b, &v1_b_rx, takeover));
    assert!(v1_a.input_sink(&a_attach).is_none());
    assert!(v2.input_sink(&b_attach).is_some());
}

/// A reply from the attach that just lost authority still reaches the PTY
/// inside the grace window, and not after it.
#[tokio::test]
async fn authority_grace_keeps_the_previous_answerers_reply() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, _) = open_control_v2(&mut server, "a");
    let (b, b_rx, _) = open_control_v2(&mut server, "b");
    let a_attach = attach_id_of(&attach(&mut server, &a, &a_rx, attach_params(&pane_id)));
    let _b_attach = attach_id_of(&attach(&mut server, &b, &b_rx, attach_params(&pane_id)));
    set_tab_geometry(&mut server, &a, &tab_id, 120, 40);
    store_tab_geometry(&mut server, &b, &tab_id, 100, 30);
    assert!(a.input_authority_for_test(&a_attach));

    let claimed = claim_tab_geometry(
        &mut server,
        &b,
        TabClaimGeometryParams {
            tab_id: Some(tab_id.clone()),
            attach_id: None,
        },
    );
    assert_eq!(claimed["result"]["type"], "ok", "{claimed}");
    assert!(!a.input_authority_for_test(&a_attach), "b answers now");

    let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel::<api::ApiRequestMessage>();
    let reply = TerminalInputParams {
        attach_id: a_attach.clone(),
        bytes: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"\x1b[?1;2c"),
        auto: true,
    };
    let response =
        crate::api::control_stream_terminal_input(&a, &api_tx, "i1".into(), reply.clone());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["result"]["type"],
        "ok"
    );
    assert_eq!(
        input_rx.try_recv().expect("the outstanding reply lands"),
        bytes::Bytes::from_static(b"\x1b[?1;2c")
    );
    assert!(api_rx.try_recv().is_err(), "a reply never claims");

    a.backdate_authority_loss_for_test(
        &a_attach,
        crate::api::control::AUTHORITY_GRACE + std::time::Duration::from_secs(1),
    );
    let response = crate::api::control_stream_terminal_input(&a, &api_tx, "i2".into(), reply);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["result"]["type"],
        "ok"
    );
    assert!(
        input_rx.try_recv().is_err(),
        "past the window the stale answerer is silent"
    );
}

/// A protocol 1 stream cannot be told to stop answering, so attaching again
/// to the same terminal replaces its earlier attach.
#[tokio::test]
async fn protocol_one_reattach_replaces_the_earlier_attach() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, _) = focused_ids(&server);
    let (v1, v1_rx) = open_control(&mut server);
    let first = attach_id_of(&attach(&mut server, &v1, &v1_rx, attach_params(&pane_id)));
    drain(&v1_rx);

    // The eviction record precedes the new attach's response on the lane.
    let response = send_control(
        &mut server,
        &v1,
        Method::TerminalAttach(attach_params(&pane_id)),
    );
    assert!(response.is_none(), "{response:?}");
    let records = drain(&v1_rx);
    assert!(
        records_of(&records, "terminal.detached")
            .any(|record| record["attach_id"] == first && record["reason"] == "takeover"),
        "{records:?}"
    );
    let second = attach_id_of(
        records
            .iter()
            .find(|record| record["result"]["type"] == "terminal_attached")
            .expect("attach response"),
    );
    assert_ne!(first, second);
    assert!(v1.input_sink(&first).is_none());
    assert!(v1.input_sink(&second).is_some());

    let listed = send_control(&mut server, &v1, Method::ControlList(Default::default()))
        .expect("list response");
    let attaches = listed["result"]["connections"][0]["attaches"]
        .as_array()
        .expect("attaches");
    assert_eq!(attaches.len(), 1, "{listed}");
    assert_eq!(attaches[0]["attach_id"], second);
    assert_eq!(
        focused_runtime(&server)
            .raw_query_authority_for_test()
            .as_deref(),
        Some(second.as_str())
    );
}

/// Replacing its own attach is not a new exclusivity claim: shared protocol
/// 2 viewers stay, and the protocol 1 stream keeps answering.
#[tokio::test]
async fn protocol_one_reattach_keeps_shared_viewers() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, _) = focused_ids(&server);
    let (v1, v1_rx) = open_control(&mut server);
    let first = attach_id_of(&attach(&mut server, &v1, &v1_rx, attach_params(&pane_id)));
    let (v2, v2_rx, _) = open_control_v2(&mut server, "viewer");
    let viewer = attach_id_of(&attach(&mut server, &v2, &v2_rx, attach_params(&pane_id)));
    drain(&v1_rx);
    drain(&v2_rx);

    let response = send_control(
        &mut server,
        &v1,
        Method::TerminalAttach(attach_params(&pane_id)),
    );
    assert!(response.is_none(), "{response:?}");
    let records = drain(&v1_rx);
    assert!(
        records_of(&records, "terminal.detached")
            .any(|record| record["attach_id"] == first && record["reason"] == "takeover"),
        "{records:?}"
    );
    let second = attach_id_of(
        records
            .iter()
            .find(|record| record["result"]["type"] == "terminal_attached")
            .expect("attach response"),
    );
    let viewer_records = drain(&v2_rx);
    assert!(
        records_of(&viewer_records, "terminal.detached")
            .next()
            .is_none(),
        "the shared viewer stays: {viewer_records:?}"
    );

    let listed = send_control(&mut server, &v1, Method::ControlList(Default::default()))
        .expect("list response");
    let connections = listed["result"]["connections"].as_array().expect("list");
    let attaches_of = |connection: &serde_json::Value| {
        connection["attaches"]
            .as_array()
            .expect("attaches")
            .iter()
            .map(|attach| attach["attach_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let by_id = |id: &str| {
        connections
            .iter()
            .find(|connection| attaches_of(connection).iter().any(|attach| attach == id))
            .expect("connection")
    };
    assert_eq!(
        attaches_of(by_id(&second)),
        vec![second.clone()],
        "{listed}"
    );
    assert_eq!(
        attaches_of(by_id(&viewer)),
        vec![viewer.clone()],
        "{listed}"
    );
    assert_eq!(
        focused_runtime(&server)
            .raw_query_authority_for_test()
            .as_deref(),
        Some(second.as_str())
    );
}

/// A refused replacement must leave the old attach alone: the conflict is
/// checked before anything is retired.
#[tokio::test]
async fn refused_protocol_one_reattach_keeps_the_existing_attach() {
    let mut server = test_headless_server();
    let _input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, _) = focused_ids(&server);
    let (v1, v1_rx) = open_control(&mut server);
    let attached = attach(&mut server, &v1, &v1_rx, attach_params(&pane_id));
    let first = attach_id_of(&attached);
    let terminal_id = attached["result"]["terminal_id"]
        .as_str()
        .expect("terminal id")
        .to_owned();
    drain(&v1_rx);
    // A direct client sizes the terminal now.
    server.terminal_attach_owners.insert(terminal_id, 42);

    let response = send_control(
        &mut server,
        &v1,
        Method::TerminalAttach(attach_params(&pane_id)),
    )
    .expect("refusal answers through the app");
    assert_eq!(response["error"]["code"], "terminal_attached", "{response}");
    let records = drain(&v1_rx);
    assert!(
        records_of(&records, "terminal.detached").next().is_none(),
        "{records:?}"
    );
    assert!(v1.input_sink(&first).is_some());
    let listed = send_control(&mut server, &v1, Method::ControlList(Default::default()))
        .expect("list response");
    let attaches = listed["result"]["connections"][0]["attaches"]
        .as_array()
        .expect("attaches");
    assert_eq!(attaches.len(), 1, "{listed}");
    assert_eq!(attaches[0]["attach_id"], first);
}

/// An emulator's own reply is forwarded only by the query authority and is
/// never interaction, so it cannot claim the tab.
#[tokio::test]
async fn auto_input_only_from_the_authority() {
    let mut server = test_headless_server();
    let mut input_rx = install_focused_test_runtime(&mut server, b"");
    let (pane_id, tab_id) = focused_ids(&server);
    let (a, a_rx, _) = open_control_v2(&mut server, "a");
    let (b, b_rx, _) = open_control_v2(&mut server, "b");
    let a_attach = attach_id_of(&attach(&mut server, &a, &a_rx, attach_params(&pane_id)));
    let b_attach = attach_id_of(&attach(&mut server, &b, &b_rx, attach_params(&pane_id)));
    set_tab_geometry(&mut server, &a, &tab_id, 120, 40);
    store_tab_geometry(&mut server, &b, &tab_id, 100, 30);
    assert!(
        a.input_authority_for_test(&a_attach),
        "the owner's attach answers"
    );
    assert!(!b.input_authority_for_test(&b_attach));

    let (api_tx, mut api_rx) = tokio::sync::mpsc::unbounded_channel::<api::ApiRequestMessage>();
    let reply = |attach_id: &str, auto: bool| TerminalInputParams {
        attach_id: attach_id.to_owned(),
        bytes: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, b"\x1b[?1;2c"),
        auto,
    };

    // The non-authority's reply is dropped, and its armed claim stays armed.
    let response =
        crate::api::control_stream_terminal_input(&b, &api_tx, "i1".into(), reply(&b_attach, true));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["result"]["type"],
        "ok"
    );
    assert!(
        input_rx.try_recv().is_err(),
        "a duplicate reply never reaches the PTY"
    );
    assert!(
        api_rx.try_recv().is_err(),
        "an automatic reply never claims"
    );
    assert!(
        b.take_claim_on_input(&b_attach),
        "the claim waits for real typing"
    );
    b.set_claim_on_input([b_attach.clone()].into_iter().collect());

    // The authority's reply reaches the PTY, still without claiming.
    let response =
        crate::api::control_stream_terminal_input(&a, &api_tx, "i2".into(), reply(&a_attach, true));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["result"]["type"],
        "ok"
    );
    assert_eq!(
        input_rx.try_recv().expect("pty input"),
        bytes::Bytes::from_static(b"\x1b[?1;2c")
    );
    assert!(api_rx.try_recv().is_err());

    // Real typing from the non-owner claims and reaches the PTY. The claim
    // is a blocking round trip to the app thread, so answer it here.
    let app_thread = std::thread::spawn(move || {
        let claim = api_rx.blocking_recv().expect("typing dispatches the claim");
        let claimed = matches!(claim.request.method, Method::TabClaimGeometry(_));
        let _ = claim
            .respond_to
            .send(r#"{"id":"i3:claim","result":{"type":"ok"}}"#.to_owned());
        claimed
    });
    let response = crate::api::control_stream_terminal_input(
        &b,
        &api_tx,
        "i3".into(),
        reply(&b_attach, false),
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["result"]["type"],
        "ok"
    );
    assert_eq!(
        input_rx.try_recv().expect("pty input"),
        bytes::Bytes::from_static(b"\x1b[?1;2c")
    );
    assert!(
        app_thread.join().expect("app thread"),
        "typing dispatches the claim"
    );
}

#[tokio::test]
async fn independent_pane_geometry_preserves_shared_viewers_and_handoffs() {
    use crate::api::schema::{PaneTerminalSize, TabSetPaneGeometryParams};
    use ratatui::layout::Direction;
    let mut server = test_headless_server();
    let mut ws = crate::workspace::Workspace::test_new("independent-fonts");
    let left = ws.tabs[0].root_pane;
    let upper = ws.test_split(Direction::Horizontal);
    let lower = ws.test_split(Direction::Vertical);
    let mut inputs = Vec::new();
    for pane in [left, upper, lower] {
        let (runtime, input) =
            crate::terminal::TerminalRuntime::test_with_channel_and_scrollback_bytes(
                80,
                24,
                0,
                b"shared output",
                4,
            );
        ws.insert_test_runtime(pane, runtime);
        inputs.push(input);
    }
    server.app.state.workspaces = vec![ws];
    server.app.state.active = Some(0);
    server.app.state.selected = 0;
    server.app.state.mode = crate::app::Mode::Terminal;
    let tab = server.app.public_tab_id(0, 0).unwrap();
    let ids: Vec<_> = [left, upper, lower]
        .into_iter()
        .map(|pane| server.app.public_pane_id(0, pane).unwrap())
        .collect();
    let open_v3 = |server: &mut HeadlessServer, name: &str| {
        open_control_with(
            server,
            ControlOpenParams {
                client: Some(ControlClientInfo {
                    name: name.into(),
                    version: "test".into(),
                    protocol: 3,
                }),
            },
        )
    };
    let (a, a_rx, a_id) = open_v3(&mut server, "owner");
    let (b, b_rx, b_id) = open_v3(&mut server, "viewer");
    let (old, old_rx, _) = open_control_v2(&mut server, "older-viewer");
    for (handle, rx) in [(&a, &a_rx), (&b, &b_rx), (&old, &old_rx)] {
        for pane in &ids {
            attach(&mut server, handle, rx, attach_params(pane));
            drain(rx);
        }
    }
    let sizes: std::collections::BTreeMap<_, _> = ids
        .iter()
        .cloned()
        .zip([
            PaneTerminalSize {
                cols: 60,
                rows: 30,
                cell_width_px: 12,
                cell_height_px: 24,
            },
            PaneTerminalSize {
                cols: 80,
                rows: 20,
                cell_width_px: 8,
                cell_height_px: 16,
            },
            PaneTerminalSize {
                cols: 80,
                rows: 10,
                cell_width_px: 8,
                cell_height_px: 16,
            },
        ])
        .collect();
    let response = send_control(
        &mut server,
        &a,
        Method::TabSetPaneGeometry(TabSetPaneGeometryParams {
            geometry: geometry_params(&tab, 120, 40, true),
            panes: sizes.clone(),
        }),
    )
    .unwrap();
    assert_eq!(response["result"]["type"], "ok", "{response}");
    assert_eq!(server.tab_geometry_controllers.get(&tab), Some(&a_id));
    for (pane, id) in [left, upper, lower].into_iter().zip(&ids) {
        let size = sizes[id];
        assert_eq!(
            server.app.state.workspaces[0].test_runtimes[&pane].current_size(),
            (size.rows, size.cols)
        );
    }
    let current = drain(&b_rx);
    let layout = &records_of(&current, "tab.layout").last().unwrap()["layout"];
    assert_eq!(layout["area"]["width"], 120);
    assert!(layout["panes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|pane| pane["terminal_size"].is_object()));
    assert!(!current
        .iter()
        .any(|record| record["type"] == "terminal.detached"));
    let legacy = drain(&old_rx);
    let projected = &records_of(&legacy, "tab.layout").last().unwrap()["layout"];
    assert_eq!(projected["area"]["width"], 140);
    assert_eq!(projected["area"]["height"], 30);
    for pane in projected["panes"].as_array().unwrap() {
        let size = sizes[pane["pane_id"].as_str().unwrap()];
        assert_eq!(pane["rect"]["width"], size.cols);
        assert_eq!(pane["rect"]["height"], size.rows);
        assert!(pane.get("terminal_size").is_none());
    }
    let snapshot = send_control(
        &mut server,
        &old,
        Method::SessionSnapshot(Default::default()),
    )
    .unwrap();
    assert_eq!(&snapshot["result"]["snapshot"]["layouts"][0], projected);
    assert!(server.app.state.direct_attach_resize_locks.is_empty());
    let list = send_control(&mut server, &a, Method::ControlList(Default::default())).unwrap();
    assert!(list["result"]["connections"]
        .as_array()
        .unwrap()
        .iter()
        .all(|connection| connection["attaches"].as_array().unwrap().len() == 3));

    // Rejected requests must not change the current owner's stored grids.
    let unsupported = send_control(
        &mut server,
        &old,
        Method::TabSetPaneGeometry(TabSetPaneGeometryParams {
            geometry: geometry_params(&tab, 120, 40, true),
            panes: sizes.clone(),
        }),
    )
    .unwrap();
    assert_eq!(unsupported["error"]["code"], "unsupported_protocol");
    for (cols, rows) in [(0, 30), (1, 30), (3, 30), (60, 0), (60, 1)] {
        let mut invalid = sizes.clone();
        let size = invalid.get_mut(&ids[0]).unwrap();
        size.cols = cols;
        size.rows = rows;
        let rejected = send_control(
            &mut server,
            &b,
            Method::TabSetPaneGeometry(TabSetPaneGeometryParams {
                geometry: geometry_params(&tab, 120, 40, true),
                panes: invalid,
            }),
        )
        .unwrap();
        assert_eq!(rejected["error"]["code"], "invalid_request");
        assert_eq!(server.tab_geometry_controllers.get(&tab), Some(&a_id));
        assert_eq!(
            server.app.state.control_pane_geometry[&left],
            sizes[&ids[0]]
        );
        assert_eq!(
            server.app.state.workspaces[0].test_runtimes[&left].current_size(),
            (30, 60)
        );
    }

    // The minimum accepted grid must be the same in the layout and runtime.
    let mut minimum = sizes.clone();
    minimum.get_mut(&ids[0]).unwrap().cols = 4;
    minimum.get_mut(&ids[0]).unwrap().rows = 2;
    drain(&a_rx);
    let accepted = send_control(
        &mut server,
        &a,
        Method::TabSetPaneGeometry(TabSetPaneGeometryParams {
            geometry: geometry_params(&tab, 120, 40, true),
            panes: minimum,
        }),
    )
    .unwrap();
    assert_eq!(accepted["result"]["type"], "ok");
    let records = drain(&a_rx);
    let layout = &records_of(&records, "tab.layout").last().unwrap()["layout"];
    let pane = layout["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["pane_id"] == ids[0])
        .unwrap();
    assert_eq!(pane["terminal_size"]["cols"], 4);
    assert_eq!(pane["terminal_size"]["rows"], 2);
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&left].current_size(),
        (2, 4)
    );
    send_control(
        &mut server,
        &a,
        Method::TabSetPaneGeometry(TabSetPaneGeometryParams {
            geometry: geometry_params(&tab, 120, 40, true),
            panes: sizes.clone(),
        }),
    );
    let rejected = send_control(
        &mut server,
        &a,
        Method::TabSetGeometry(geometry_params(&tab, 0, 40, true)),
    )
    .unwrap();
    assert_eq!(rejected["error"]["code"], "invalid_request");
    assert_eq!(server.app.state.control_pane_geometry[&left].rows, 30);

    let mut other_sizes = sizes.clone();
    other_sizes.get_mut(&ids[0]).unwrap().rows = 25;
    send_control(
        &mut server,
        &b,
        Method::TabSetPaneGeometry(TabSetPaneGeometryParams {
            geometry: geometry_params(&tab, 120, 40, false),
            panes: other_sizes,
        }),
    );
    assert_eq!(
        server.tab_geometry_controllers.get(&tab),
        Some(&a_id),
        "storing viewer geometry must not claim"
    );
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&left].current_size(),
        (30, 60)
    );
    claim_tab_geometry(
        &mut server,
        &b,
        TabClaimGeometryParams {
            tab_id: Some(tab.clone()),
            attach_id: None,
        },
    );
    assert_eq!(server.tab_geometry_controllers.get(&tab), Some(&b_id));
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&left].current_size(),
        (25, 60)
    );
    // A legacy viewer can still claim using the original uniform-grid method.
    set_tab_geometry(&mut server, &old, &tab, 120, 40);
    assert!(server.app.state.control_pane_geometry.is_empty());
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&left].current_size(),
        (40, 60)
    );
    let list = send_control(&mut server, &a, Method::ControlList(Default::default())).unwrap();
    assert!(list["result"]["connections"]
        .as_array()
        .unwrap()
        .iter()
        .all(|connection| connection["attaches"].as_array().unwrap().len() == 3));
    // Native zoom only changes the visible terminal; unzoom restores all
    // independent grids, without converting any attachment to exclusive mode.
    server.app.state.workspaces[0].tabs[0].zoomed = true;
    let zoom_size = PaneTerminalSize {
        cols: 100,
        rows: 50,
        cell_width_px: 8,
        cell_height_px: 16,
    };
    send_control(
        &mut server,
        &a,
        Method::TabSetPaneGeometry(TabSetPaneGeometryParams {
            geometry: geometry_params(&tab, 120, 60, true),
            panes: [(ids[2].clone(), zoom_size)].into_iter().collect(),
        }),
    );
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&lower].current_size(),
        (50, 100)
    );
    let zoom_records = drain(&old_rx);
    let zoom_layout = &records_of(&zoom_records, "tab.layout").last().unwrap()["layout"];
    assert_eq!(zoom_layout["panes"].as_array().unwrap().len(), 1);
    assert_eq!(zoom_layout["area"]["width"], 100);
    assert_eq!(zoom_layout["area"]["height"], 50);
    server.app.state.workspaces[0].tabs[0].zoomed = false;
    send_control(
        &mut server,
        &a,
        Method::TabSetPaneGeometry(TabSetPaneGeometryParams {
            geometry: geometry_params(&tab, 120, 40, true),
            panes: sizes,
        }),
    );
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&left].current_size(),
        (30, 60)
    );
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&lower].current_size(),
        (10, 80)
    );
    // A departing owner hands off to the most recently active surviving
    // viewer, which here is the legacy client that last claimed the tab.
    close_control(&mut server, &a);
    assert!(server.app.state.control_pane_geometry.is_empty());
    assert_eq!(
        server.app.state.workspaces[0].test_runtimes[&left].current_size(),
        (40, 60)
    );
    drop(inputs);
}
