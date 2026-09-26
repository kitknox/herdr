//! Server side of control streams: raw terminal attaches, their sharing,
//! and tab geometry ownership for clients that render panes themselves.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use ratatui::layout::Rect;
use tracing::info;

use crate::api;
use crate::api::control::{ControlConnectionHandle, CONTROL_STREAM_PROTOCOL};
use crate::api::schema::{
    ControlAttachInfo, ControlClientInfo, ControlConnectionInfo, ControlTabInfo, ErrorBody,
    ErrorResponse, GeometryController, GeometryControllerKind, ResponseResult, SuccessResponse,
    TabChrome, TabClaimGeometryParams, TabSetGeometryParams, TerminalAttachGeometry,
    TerminalAttachParams, TerminalAttachTarget, TerminalDetachReason, TerminalQueryAuthority,
    TerminalResizeParams,
};
use crate::pane::raw_stream::{RawTapBudget, DEFAULT_TAP_BUDGET_BYTES};
use crate::protocol::ServerMessage;

use super::HeadlessServer;

/// Control connection ids live above every client id so ownership maps can
/// hold both.
const CONTROL_CONNECTION_ID_BASE: u64 = 1 << 40;
const DEFAULT_HISTORY_LIMIT_BYTES: usize = 1024 * 1024;
const MAX_HISTORY_LIMIT_BYTES: usize = 8 * 1024 * 1024;

pub(super) fn is_control_connection_id(id: u64) -> bool {
    id >= CONTROL_CONNECTION_ID_BASE
}

/// Which control attaches a takeover evicts from a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EvictScope {
    /// Every attach: a size owner is arriving.
    All,
    /// A tab follower is arriving: size owners go, and so do followers on
    /// protocol 1 connections, which expect exclusive ownership.
    TabFollowerTakeover,
}

impl EvictScope {
    fn evicts(self, geometry: TerminalAttachGeometry, protocol: u32) -> bool {
        match self {
            Self::All => true,
            Self::TabFollowerTakeover => {
                geometry == TerminalAttachGeometry::Terminal || protocol <= 1
            }
        }
    }
}

/// Size and chrome a control connection asked for on one tab, whether or
/// not it currently owns the tab.
#[derive(Debug, Clone, Copy)]
pub(super) struct ControlTabGeometry {
    cols: u16,
    rows: u16,
    cell_size: crate::kitty_graphics::HostCellSize,
    chrome: TabChrome,
    /// Activity stamp of the last claim or input from this connection on
    /// the tab; the successor on owner disconnect is the highest.
    last_interaction: u64,
}

pub(super) struct ControlConnectionState {
    handle: ControlConnectionHandle,
    attaches: HashMap<String, ControlAttach>,
    tab_geometry: HashMap<String, ControlTabGeometry>,
    tab_pane_geometry:
        HashMap<String, std::collections::BTreeMap<String, api::schema::PaneTerminalSize>>,
    next_attach: u64,
    client: Option<ControlClientInfo>,
    /// Negotiated control stream protocol, 1 for clients that sent none.
    protocol: u32,
}

impl ControlConnectionState {
    /// Whether this connection stored a size for the tab, owner or not.
    pub(super) fn holds_tab(&self, tab_id: &str) -> bool {
        self.tab_geometry.contains_key(tab_id)
    }

    pub(super) fn tab_geometry(
        &self,
        tab_id: &str,
    ) -> Option<(u16, u16, crate::kitty_graphics::HostCellSize)> {
        self.tab_geometry
            .get(tab_id)
            .map(|geometry| (geometry.cols, geometry.rows, geometry.cell_size))
    }
}

struct ControlAttach {
    terminal_id: String,
    geometry: TerminalAttachGeometry,
    history_limit_bytes: usize,
    answer_queries: TerminalQueryAuthority,
    answers_queries: bool,
    /// Creation order within the connection; the oldest attach on a
    /// terminal answers queries when nothing else decides.
    seq: u64,
}

fn error(id: String, code: &str, message: String) -> String {
    serde_json::to_string(&ErrorResponse {
        id,
        error: ErrorBody {
            code: code.into(),
            message,
        },
    })
    .unwrap_or_else(|_| "{}".to_string())
}

fn success(id: String, result: ResponseResult) -> String {
    serde_json::to_string(&SuccessResponse { id, result }).unwrap_or_else(|_| "{}".to_string())
}

impl HeadlessServer {
    /// Handles control-stream methods before the app sees them. Returns the
    /// response when the request was one of them.
    pub(super) fn handle_control_api_request(
        &mut self,
        msg: &api::ApiRequestMessage,
    ) -> Option<String> {
        use api::schema::Method;

        let id = msg.request.id.clone();
        let response = match &msg.request.method {
            Method::ControlOpen(params) => {
                self.control_open(id, msg.control.as_ref(), params.client.clone())
            }
            Method::ControlClose(_) => self.control_close(id, msg.control.as_ref()),
            Method::ControlList(_) => self.control_list(id, msg.control.as_ref()),
            Method::TerminalAttach(params) => {
                self.control_terminal_attach(id, msg.control.as_ref(), params)
            }
            Method::TerminalDetach(params) => {
                self.control_terminal_detach(id, msg.control.as_ref(), params)
            }
            Method::TerminalSnapshot(params) => {
                self.control_terminal_snapshot(id, msg.control.as_ref(), params)
            }
            Method::TerminalResize(params) => {
                self.control_terminal_resize(id, msg.control.as_ref(), params)
            }
            Method::TerminalInput(_) => error(
                id,
                "control_stream_required",
                "terminal.input is only available on a control stream".into(),
            ),
            Method::TabSetGeometry(params) => {
                self.control_tab_set_geometry(id, msg.control.as_ref(), params, None)
            }
            Method::TabSetPaneGeometry(params) => {
                self.control_tab_set_pane_geometry(id, msg.control.as_ref(), params)
            }
            Method::TabClaimGeometry(params) => {
                self.control_tab_claim_geometry(id, msg.control.as_ref(), params)
            }
            // A stream bootstraps from the same rectangles its `tab.layout`
            // records carry, not the TUI's view area.
            Method::SessionSnapshot(_) if msg.control.as_ref().is_some_and(|h| h.id() != 0) => {
                self.control_session_snapshot(id, msg.control.as_ref())
            }
            _ => return None,
        };
        Some(response)
    }

    fn control_session_snapshot(
        &self,
        id: String,
        handle: Option<&ControlConnectionHandle>,
    ) -> String {
        let protocol = handle
            .and_then(|h| self.control_connections.get(&h.id()))
            .map_or(1, |s| s.protocol);
        let mut snapshot = self.app.session_snapshot();
        snapshot.layouts = self
            .all_tab_targets()
            .into_iter()
            .filter_map(|target| {
                self.control_tab_layout(target, self.current_tab_area(target))
                    .map(|layout| self.layout_for_control_protocol(layout, protocol))
            })
            .collect();
        success(
            id,
            ResponseResult::SessionSnapshot {
                snapshot: Box::new(snapshot),
            },
        )
    }

    /// The area a tab is laid out in right now: its control owner's stored
    /// size, else its owning shell client's, else the size every tab gets
    /// from the foreground client or the headless default.
    pub(super) fn current_tab_area(&self, target: crate::ui::TabSurfaceTarget) -> Rect {
        if let Some(tab_id) = self.tab_id_for_target(target) {
            if let Some(&controller) = self.tab_geometry_controllers.get(&tab_id) {
                if let Some((cols, rows, _)) =
                    self.control_tab_geometry_for_target(controller, target)
                {
                    return Rect::new(0, 0, cols, rows);
                }
                if let Some(client) = self.clients.get(&controller) {
                    let (cols, rows) = client.terminal_size;
                    return Rect::new(0, 0, cols, rows);
                }
            }
        }
        let (cols, rows) = self.effective_size;
        Rect::new(0, 0, cols, rows)
    }

    /// The layout a `tab.layout` record carries for `target` laid out in `area`.
    pub(super) fn control_tab_layout(
        &self,
        target: crate::ui::TabSurfaceTarget,
        area: Rect,
    ) -> Option<api::schema::PaneLayoutSnapshot> {
        let surface = crate::ui::compute_tab_surface_for(
            &self.app.state,
            &self.app.terminal_runtimes,
            Some(target),
            area,
            false,
            crate::kitty_graphics::HostCellSize::default(),
        );
        let mut layout = self.app.control_tab_layout_snapshot(
            target.workspace_index,
            target.tab_index,
            area,
            &surface.pane_infos,
        )?;
        for (pane, info) in layout.panes.iter_mut().zip(&surface.pane_infos) {
            pane.terminal_size = self.app.state.control_pane_geometry.get(&info.id).copied();
        }
        Some(layout)
    }

    fn layout_for_control_protocol(
        &self,
        layout: api::schema::PaneLayoutSnapshot,
        protocol: u32,
    ) -> api::schema::PaneLayoutSnapshot {
        if protocol >= 3 {
            return layout;
        }
        let Some((ws, tab)) = self.app.parse_tab_id(&layout.tab_id) else {
            return layout;
        };
        super::control_geometry::legacy_layout(
            layout,
            self.app.state.workspaces[ws].tabs[tab].layout.root(),
            |pane| self.app.public_pane_id(ws, pane),
        )
    }

    fn control_connection_id(
        &mut self,
        handle: Option<&ControlConnectionHandle>,
    ) -> Result<u64, String> {
        let Some(handle) = handle else {
            return Err("this method is only available on a control stream".into());
        };
        let id = handle.id();
        if id == 0 || !self.control_connections.contains_key(&id) {
            return Err("control stream is not open".into());
        }
        self.prune_closed_control_attaches(id);
        Ok(id)
    }

    fn control_open(
        &mut self,
        id: String,
        handle: Option<&ControlConnectionHandle>,
        client: Option<ControlClientInfo>,
    ) -> String {
        let Some(handle) = handle else {
            return error(
                id,
                "control_stream_required",
                "control.open must be the first request on its own connection".into(),
            );
        };
        if handle.id() != 0 {
            return error(
                id,
                "unsupported_in_control_stream",
                "control stream is already open".into(),
            );
        }
        let connection_id = CONTROL_CONNECTION_ID_BASE + self.next_control_connection_id;
        self.next_control_connection_id += 1;
        handle.assign_id(connection_id);
        // No declared protocol means the original contract: one owner per
        // terminal, no authority records.
        let protocol = client
            .as_ref()
            .map(|client| client.protocol.clamp(1, CONTROL_STREAM_PROTOCOL))
            .unwrap_or(1);
        handle.set_protocol(protocol);
        self.control_connections.insert(
            connection_id,
            ControlConnectionState {
                handle: handle.clone(),
                attaches: HashMap::new(),
                tab_geometry: HashMap::new(),
                tab_pane_geometry: HashMap::new(),
                next_attach: 0,
                client: client.clone(),
                protocol,
            },
        );
        info!(
            connection_id,
            protocol,
            client = client
                .as_ref()
                .map(|client| client.name.as_str())
                .unwrap_or(""),
            "control stream opened"
        );
        success(
            id,
            ResponseResult::ControlOpened {
                connection_id,
                boot_id: self.client_shell_boot_id.clone(),
                version: crate::build_info::version(),
                protocol: crate::protocol::PROTOCOL_VERSION,
                capabilities: api::default_server_capabilities(),
                control_protocol: protocol,
            },
        )
    }

    fn control_close(&mut self, id: String, handle: Option<&ControlConnectionHandle>) -> String {
        let Some(handle) = handle else {
            return error(
                id,
                "control_stream_required",
                "control.close is only available on a control stream".into(),
            );
        };
        let connection_id = handle.id();
        if self.control_connections.contains_key(&connection_id) {
            self.release_control_connection(connection_id);
            info!(connection_id, "control stream closed");
        }
        success(id, ResponseResult::Ok {})
    }

    fn control_list(&mut self, id: String, handle: Option<&ControlConnectionHandle>) -> String {
        let self_connection_id = handle
            .map(ControlConnectionHandle::id)
            .filter(|id| *id != 0);
        let mut ids = self.control_connections.keys().copied().collect::<Vec<_>>();
        ids.sort_unstable();
        let connections = ids
            .into_iter()
            .filter_map(|connection_id| {
                let state = self.control_connections.get(&connection_id)?;
                let mut attaches = state
                    .attaches
                    .iter()
                    .map(|(attach_id, attach)| {
                        (
                            attach.seq,
                            ControlAttachInfo {
                                attach_id: attach_id.clone(),
                                terminal_id: attach.terminal_id.clone(),
                                pane_id: self
                                    .app
                                    .resolve_terminal_target(&attach.terminal_id)
                                    .ok()
                                    .and_then(|target| {
                                        self.app.public_pane_id(target.ws_idx, target.pane_id)
                                    }),
                                geometry: attach.geometry,
                                answer_queries: attach.answer_queries,
                                answers_queries: attach.answers_queries,
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                attaches.sort_unstable_by_key(|(seq, _)| *seq);
                let mut tabs = state
                    .tab_geometry
                    .iter()
                    .map(|(tab_id, geometry)| ControlTabInfo {
                        tab_id: tab_id.clone(),
                        cols: geometry.cols,
                        rows: geometry.rows,
                        cell_width_px: geometry.cell_size.width_px,
                        cell_height_px: geometry.cell_size.height_px,
                        chrome: geometry.chrome,
                        controller: self.tab_geometry_controllers.get(tab_id)
                            == Some(&connection_id),
                    })
                    .collect::<Vec<_>>();
                tabs.sort_unstable_by(|left, right| left.tab_id.cmp(&right.tab_id));
                Some(ControlConnectionInfo {
                    connection_id,
                    control_protocol: state.protocol,
                    client: state.client.clone(),
                    attaches: attaches.into_iter().map(|(_, info)| info).collect(),
                    tabs,
                })
            })
            .collect();
        success(
            id,
            ResponseResult::ControlList {
                self_connection_id,
                connections,
            },
        )
    }

    /// Drops every attach and geometry claim of a control connection. Tabs
    /// it owned pass to the connection that last interacted with them, else
    /// back to the shell client rules.
    pub(super) fn release_control_connection(&mut self, connection_id: u64) {
        let Some(state) = self.control_connections.remove(&connection_id) else {
            return;
        };
        state.handle.close();
        let mut terminals = HashSet::new();
        for (attach_id, attach) in state.attaches {
            terminals.insert(attach.terminal_id.clone());
            self.release_control_attach(
                connection_id,
                &state.handle,
                &attach_id,
                &attach,
                TerminalDetachReason::Closed,
            );
        }
        let owned = self
            .tab_geometry_controllers
            .iter()
            .filter(|(_, controller)| **controller == connection_id)
            .map(|(tab_id, _)| tab_id.clone())
            .collect::<Vec<_>>();
        self.tab_geometry_controllers
            .retain(|_, controller| *controller != connection_id);
        for tab_id in owned {
            let successor = self
                .control_connections
                .iter()
                .filter_map(|(id, state)| state.tab_geometry.get(&tab_id).map(|g| (*id, *g)))
                .max_by_key(|(_, geometry)| geometry.last_interaction);
            if let Some((successor_id, geometry)) = successor {
                self.claim_control_tab_geometry(successor_id, &tab_id, geometry);
            }
        }
        self.sync_control_geometry_tabs();
        for terminal_id in terminals {
            self.sync_query_authority(&terminal_id);
        }
        // Always re-derive geometry: terminal-sized attaches released above
        // need their tabs back even when this stream held no tab claims.
        if !self.resize_tabs_for_only_shell_client(true) {
            self.reapply_controlled_shell_tab_geometry(true);
        }
    }

    fn release_control_attach(
        &mut self,
        _connection_id: u64,
        handle: &ControlConnectionHandle,
        attach_id: &str,
        attach: &ControlAttach,
        reason: TerminalDetachReason,
    ) {
        handle.unregister_input(attach_id);
        if let Some(runtime) = self.control_runtime(&attach.terminal_id) {
            runtime.detach_raw(attach_id, reason);
        }
        if attach.geometry == TerminalAttachGeometry::Terminal {
            if let Some(terminal_id) = self.terminal_id_by_string(&attach.terminal_id) {
                self.app
                    .state
                    .direct_attach_resize_locks
                    .remove(&terminal_id);
            }
            // Hand the terminal back to whoever sizes its tab, as a direct
            // attach client's removal does.
            if let Some((controller_id, target)) =
                self.shell_geometry_controller_for_terminal(&attach.terminal_id)
            {
                self.restore_shell_tab_geometry(controller_id, target);
            } else {
                self.resize_tabs_for_only_shell_client(true);
            }
        }
    }

    /// Forget attaches whose terminal went away; the pane already sent
    /// `terminal.detached` when its runtime dropped.
    fn prune_closed_control_attaches(&mut self, connection_id: u64) {
        let Some(state) = self.control_connections.get(&connection_id) else {
            return;
        };
        let gone = state
            .attaches
            .iter()
            .filter(|(_, attach)| self.control_runtime(&attach.terminal_id).is_none())
            .map(|(attach_id, _)| attach_id.clone())
            .collect::<Vec<_>>();
        if gone.is_empty() {
            return;
        }
        for attach_id in gone {
            let Some(state) = self.control_connections.get_mut(&connection_id) else {
                return;
            };
            if state.attaches.remove(&attach_id).is_none() {
                continue;
            }
            state.handle.unregister_input(&attach_id);
        }
        self.sync_control_geometry_tabs();
    }

    /// Detaches control attaches on a terminal for a takeover, within
    /// `scope`.
    pub(super) fn detach_control_attaches_for_terminal(
        &mut self,
        terminal_id: &str,
        reason: TerminalDetachReason,
        scope: EvictScope,
    ) {
        let targets = self
            .control_connections
            .iter()
            .flat_map(|(&connection_id, state)| {
                state
                    .attaches
                    .iter()
                    .filter(|(_, attach)| {
                        attach.terminal_id == terminal_id
                            && scope.evicts(attach.geometry, state.protocol)
                    })
                    .map(move |(attach_id, _)| (connection_id, attach_id.clone()))
            })
            .collect::<Vec<_>>();
        for (connection_id, attach_id) in targets {
            let Some(state) = self.control_connections.get_mut(&connection_id) else {
                continue;
            };
            let Some(attach) = state.attaches.remove(&attach_id) else {
                continue;
            };
            let handle = state.handle.clone();
            self.release_control_attach(connection_id, &handle, &attach_id, &attach, reason);
        }
        self.sync_query_authority(terminal_id);
        self.sync_control_geometry_tabs();
    }

    /// Control attaches on a terminal: connection, attach id, geometry,
    /// protocol.
    fn control_attaches_for_terminal(
        &self,
        terminal_id: &str,
    ) -> Vec<(u64, String, TerminalAttachGeometry, u32)> {
        self.control_connections
            .iter()
            .flat_map(|(&connection_id, state)| {
                state
                    .attaches
                    .iter()
                    .filter(|(_, attach)| attach.terminal_id == terminal_id)
                    .map(move |(attach_id, attach)| {
                        (
                            connection_id,
                            attach_id.clone(),
                            attach.geometry,
                            state.protocol,
                        )
                    })
            })
            .collect()
    }

    pub(super) fn terminal_has_control_attaches(&self, terminal_id: &str) -> bool {
        self.control_connections.values().any(|state| {
            state
                .attaches
                .values()
                .any(|attach| attach.terminal_id == terminal_id)
        })
    }

    /// A direct client or any control attach holds the terminal.
    pub(super) fn terminal_is_attached(&self, terminal_id: &str) -> bool {
        self.terminal_attach_owners.contains_key(terminal_id)
            || self.terminal_has_control_attaches(terminal_id)
    }

    fn control_terminal_attach(
        &mut self,
        id: String,
        handle: Option<&ControlConnectionHandle>,
        params: &TerminalAttachParams,
    ) -> String {
        let connection_id = match self.control_connection_id(handle) {
            Ok(connection_id) => connection_id,
            Err(message) => return error(id, "control_stream_required", message),
        };
        let (terminal_id, pane_id) = match self.app.resolve_terminal_target(&params.target) {
            Ok(target) => (
                target.terminal_id,
                self.app.public_pane_id(target.ws_idx, target.pane_id),
            ),
            Err(crate::app::terminal_targets::TerminalTargetError::Ambiguous {
                target, ..
            }) => {
                return error(
                    id,
                    "ambiguous_target",
                    format!("terminal target {target} matches more than one terminal"),
                );
            }
            Err(crate::app::terminal_targets::TerminalTargetError::NotFound { .. }) => {
                if self.control_runtime(&params.target).is_some() {
                    (params.target.clone(), None)
                } else {
                    return error(
                        id,
                        "not_found",
                        format!("terminal target {} not found", params.target),
                    );
                }
            }
        };
        if self
            .pending_alt_screen_reads
            .iter()
            .any(|pending| pending.terminal_id.to_string() == terminal_id)
        {
            return error(
                id,
                "terminal_busy",
                format!("terminal {terminal_id} has a read in progress; retry"),
            );
        }

        // Protocol 2 tab followers share a terminal; a size owner (direct
        // client or a terminal-geometry attach) shares with nobody, and a
        // protocol 1 follower keeps its original one-owner contract.
        let size_owning = params.geometry == TerminalAttachGeometry::Terminal;
        let requester_protocol = self
            .control_connections
            .get(&connection_id)
            .map(|state| state.protocol)
            .unwrap_or(1);
        let direct_owner = self.terminal_attach_owners.get(&terminal_id).copied();
        let existing = self.control_attaches_for_terminal(&terminal_id);
        // A protocol 1 stream re-attaching replaces its own earlier attach, as
        // before sharing: it cannot be told to stop answering queries. The
        // replacement is validated first; a refused request keeps the old attach.
        let own = if requester_protocol <= 1 {
            existing
                .iter()
                .filter(|(owner, _, _, _)| *owner == connection_id)
                .map(|(_, attach_id, _, _)| attach_id.clone())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let others = existing
            .iter()
            .filter(|(owner, _, _, _)| *owner != connection_id)
            .collect::<Vec<_>>();
        let other_size_owner = others
            .iter()
            .any(|(_, _, geometry, _)| *geometry == TerminalAttachGeometry::Terminal);
        let conflict = if size_owning {
            direct_owner.is_some() || existing.len() > own.len()
        } else if requester_protocol <= 1 && own.is_empty() {
            direct_owner.is_some() || !others.is_empty()
        } else {
            // A protocol 2 follower, or a protocol 1 replacement: only a size
            // owner refuses; shared followers stay.
            direct_owner.is_some() || other_size_owner
        };
        if conflict && !params.takeover {
            return error(
                id,
                "terminal_attached",
                format!(
                    "terminal {terminal_id} already has an attached client; retry with takeover"
                ),
            );
        }
        if !own.is_empty() {
            for attach_id in own {
                let Some(state) = self.control_connections.get_mut(&connection_id) else {
                    break;
                };
                let Some(attach) = state.attaches.remove(&attach_id) else {
                    continue;
                };
                let handle = state.handle.clone();
                self.release_control_attach(
                    connection_id,
                    &handle,
                    &attach_id,
                    &attach,
                    TerminalDetachReason::Takeover,
                );
            }
            self.sync_query_authority(&terminal_id);
            self.sync_control_geometry_tabs();
        }
        if params.takeover {
            if let Some(owner) = direct_owner {
                self.send_to_client(
                    owner,
                    ServerMessage::ServerShutdown {
                        reason: Some("terminal attach taken over".to_owned()),
                    },
                );
                self.remove_client_and_resize_if_needed(owner);
            }
            let scope = if size_owning {
                EvictScope::All
            } else {
                EvictScope::TabFollowerTakeover
            };
            self.detach_control_attaches_for_terminal(
                &terminal_id,
                TerminalDetachReason::Takeover,
                scope,
            );
        }

        let Some(handle) = handle.cloned() else {
            return error(
                id,
                "control_stream_required",
                "terminal.attach is only available on a control stream".into(),
            );
        };
        let Some(state) = self.control_connections.get_mut(&connection_id) else {
            return error(
                id,
                "control_stream_required",
                "control stream is not open".into(),
            );
        };
        let protocol = state.protocol;
        let seq = state.next_attach;
        let attach_id = format!("{}-{}", connection_id - CONTROL_CONNECTION_ID_BASE, seq);
        state.next_attach += 1;
        let history_limit_bytes = params
            .history_limit_bytes
            .map(|limit| usize::try_from(limit).unwrap_or(usize::MAX))
            .unwrap_or(DEFAULT_HISTORY_LIMIT_BYTES)
            .min(MAX_HISTORY_LIMIT_BYTES);
        let suppress = params.answer_queries == TerminalQueryAuthority::Client;
        // Bookkeeping first: the runtime borrow below must not overlap it.
        state.attaches.insert(
            attach_id.clone(),
            ControlAttach {
                terminal_id: terminal_id.clone(),
                geometry: params.geometry,
                history_limit_bytes,
                answer_queries: params.answer_queries,
                answers_queries: false,
                seq,
            },
        );
        if params.geometry == TerminalAttachGeometry::Terminal {
            if let Some(real_terminal_id) = self.terminal_id_by_string(&terminal_id) {
                self.app
                    .state
                    .direct_attach_resize_locks
                    .insert(real_terminal_id);
            }
        }
        let Some(runtime) = self.control_runtime(&terminal_id) else {
            if let Some(state) = self.control_connections.get_mut(&connection_id) {
                state.attaches.remove(&attach_id);
            }
            return error(id, "not_found", format!("terminal {terminal_id} not found"));
        };
        runtime.attach_raw(
            attach_id.clone(),
            handle.outbound(),
            Arc::new(RawTapBudget::new(DEFAULT_TAP_BUDGET_BYTES)),
            suppress,
            protocol,
        );
        handle.register_input(attach_id.clone(), runtime.raw_input_sink());
        if params.geometry == TerminalAttachGeometry::Terminal {
            if let (Some(cols), Some(rows)) = (params.cols, params.rows) {
                runtime.resize(rows, cols, params.cell_width_px, params.cell_height_px);
            }
        }
        info!(connection_id, terminal_id = %terminal_id, attach_id = %attach_id, "control stream attached");
        // Write the response through the stream ourselves so the client
        // learns the attach id before the snapshot record that follows it.
        handle.send_line(success(
            id,
            ResponseResult::TerminalAttached {
                attach_id: attach_id.clone(),
                terminal_id: terminal_id.clone(),
                pane_id,
            },
        ));
        if !runtime.snapshot_raw(&attach_id, history_limit_bytes) {
            handle.send_line(error(
                String::new(),
                "snapshot_failed",
                format!("terminal {terminal_id} could not be snapshotted"),
            ));
        }
        // After the snapshot: a protocol 2 client reads its authority record
        // once it knows the attach.
        self.sync_query_authority(&terminal_id);
        self.sync_control_geometry_tabs();
        String::new()
    }

    /// Picks the one attach on a terminal that answers its queries: a
    /// protocol 1 attach first (it cannot be told to stop), then the tab's
    /// geometry controller, then the oldest client-authority attach.
    pub(super) fn sync_query_authority(&mut self, terminal_id: &str) {
        let controller = self
            .shell_geometry_controller_for_terminal(terminal_id)
            .map(|(controller_id, _)| controller_id);
        let mut candidates = self
            .control_connections
            .iter()
            .flat_map(|(&connection_id, state)| {
                state
                    .attaches
                    .iter()
                    .filter(|(_, attach)| {
                        attach.terminal_id == terminal_id
                            && attach.answer_queries == TerminalQueryAuthority::Client
                    })
                    .map(move |(attach_id, attach)| {
                        (connection_id, attach.seq, attach_id.clone(), state.protocol)
                    })
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by_key(|(connection_id, seq, _, _)| (*connection_id, *seq));
        let chosen = candidates
            .iter()
            .find(|(_, _, _, protocol)| *protocol <= 1)
            .or_else(|| {
                candidates
                    .iter()
                    .find(|(connection_id, _, _, _)| Some(*connection_id) == controller)
            })
            .or_else(|| candidates.first())
            .map(|(_, _, attach_id, _)| attach_id.clone());
        for state in self.control_connections.values_mut() {
            for (attach_id, attach) in state.attaches.iter_mut() {
                if attach.terminal_id == terminal_id {
                    attach.answers_queries = chosen.as_deref() == Some(attach_id.as_str());
                    // The input fast path drops automatic replies from
                    // every other attach without asking the app loop.
                    state
                        .handle
                        .set_input_authority(attach_id, attach.answers_queries);
                }
            }
        }
        if let Some(runtime) = self.control_runtime(terminal_id) {
            runtime.set_raw_query_authority(chosen.as_deref());
        }
    }

    fn control_terminal_detach(
        &mut self,
        id: String,
        handle: Option<&ControlConnectionHandle>,
        params: &TerminalAttachTarget,
    ) -> String {
        let connection_id = match self.control_connection_id(handle) {
            Ok(connection_id) => connection_id,
            Err(message) => return error(id, "control_stream_required", message),
        };
        let Some(state) = self.control_connections.get_mut(&connection_id) else {
            return error(
                id,
                "control_stream_required",
                "control stream is not open".into(),
            );
        };
        let Some(attach) = state.attaches.remove(&params.attach_id) else {
            return error(
                id,
                "unknown_attach",
                format!("attach {} is not live", params.attach_id),
            );
        };
        let handle = state.handle.clone();
        self.release_control_attach(
            connection_id,
            &handle,
            &params.attach_id,
            &attach,
            TerminalDetachReason::Closed,
        );
        self.sync_query_authority(&attach.terminal_id);
        self.sync_control_geometry_tabs();
        success(id, ResponseResult::Ok {})
    }

    fn control_terminal_snapshot(
        &mut self,
        id: String,
        handle: Option<&ControlConnectionHandle>,
        params: &TerminalAttachTarget,
    ) -> String {
        let connection_id = match self.control_connection_id(handle) {
            Ok(connection_id) => connection_id,
            Err(message) => return error(id, "control_stream_required", message),
        };
        let Some((terminal_id, history_limit_bytes)) = self
            .control_connections
            .get(&connection_id)
            .and_then(|state| state.attaches.get(&params.attach_id))
            .map(|attach| (attach.terminal_id.clone(), attach.history_limit_bytes))
        else {
            return error(
                id,
                "unknown_attach",
                format!("attach {} is not live", params.attach_id),
            );
        };
        let Some(runtime) = self.control_runtime(&terminal_id) else {
            return error(id, "not_found", format!("terminal {terminal_id} not found"));
        };
        if !runtime.snapshot_raw(&params.attach_id, history_limit_bytes) {
            return error(
                id,
                "snapshot_failed",
                format!("terminal {terminal_id} could not be snapshotted"),
            );
        }
        success(id, ResponseResult::Ok {})
    }

    fn control_terminal_resize(
        &mut self,
        id: String,
        handle: Option<&ControlConnectionHandle>,
        params: &TerminalResizeParams,
    ) -> String {
        let connection_id = match self.control_connection_id(handle) {
            Ok(connection_id) => connection_id,
            Err(message) => return error(id, "control_stream_required", message),
        };
        let Some((terminal_id, geometry)) = self
            .control_connections
            .get(&connection_id)
            .and_then(|state| state.attaches.get(&params.attach_id))
            .map(|attach| (attach.terminal_id.clone(), attach.geometry))
        else {
            return error(
                id,
                "unknown_attach",
                format!("attach {} is not live", params.attach_id),
            );
        };
        if geometry != TerminalAttachGeometry::Terminal {
            return error(
                id,
                "geometry_follows_tab",
                "this attach follows its tab layout; use tab.set_geometry".into(),
            );
        }
        if params.cols == 0 || params.rows == 0 {
            return error(
                id,
                "invalid_request",
                "terminal.resize cols and rows must be greater than 0".into(),
            );
        }
        let Some(runtime) = self.control_runtime(&terminal_id) else {
            return error(id, "not_found", format!("terminal {terminal_id} not found"));
        };
        runtime.resize(
            params.rows,
            params.cols,
            params.cell_width_px,
            params.cell_height_px,
        );
        success(id, ResponseResult::Ok {})
    }

    fn control_tab_set_pane_geometry(
        &mut self,
        id: String,
        handle: Option<&ControlConnectionHandle>,
        params: &api::schema::TabSetPaneGeometryParams,
    ) -> String {
        let connection_id = match self.control_connection_id(handle) {
            Ok(value) => value,
            Err(message) => return error(id, "control_stream_required", message),
        };
        if self
            .control_connections
            .get(&connection_id)
            .is_none_or(|state| state.protocol < 3)
        {
            return error(
                id,
                "unsupported_protocol",
                "pane geometry requires control protocol 3".into(),
            );
        }
        let Some((ws, tab)) = self.app.parse_tab_id(&params.geometry.tab_id) else {
            return error(id, "not_found", "tab not found".into());
        };
        let pane_ids: HashSet<_> = self.app.state.workspaces[ws].tabs[tab]
            .layout
            .pane_ids()
            .into_iter()
            .collect();
        // Validate the whole request before changing either stored geometry.
        if params.geometry.cols == 0
            || params.geometry.rows == 0
            || params.geometry.chrome != TabChrome::None
            || params.panes.is_empty()
            || params
                .panes
                .values()
                .map(|s| u64::from(s.cols))
                .sum::<u64>()
                > u64::from(u16::MAX)
            || params
                .panes
                .values()
                .map(|s| u64::from(s.rows))
                .sum::<u64>()
                > u64::from(u16::MAX)
            || params.panes.values().any(|size| {
                // PaneRuntime::resize clamps to 4x2. Reject smaller grids
                // before storing them or publishing a mismatched tab.layout.
                size.cols < 4
                    || size.rows < 2
                    || size.cell_width_px == 0
                    || size.cell_height_px == 0
                    || size.cell_width_px > u32::from(u16::MAX)
                    || size.cell_height_px > u32::from(u16::MAX)
            })
            || params.panes.keys().any(|pane| {
                self.app
                    .parse_pane_id(pane)
                    .is_none_or(|(pane_ws, pane_id)| pane_ws != ws || !pane_ids.contains(&pane_id))
            })
        {
            return error(
                id,
                "invalid_request",
                "pane geometry requires grids of at least 4 columns and 2 rows, with nonzero cell metrics, in a chromeless tab".into(),
            );
        }
        self.control_tab_set_geometry(id, handle, &params.geometry, Some(&params.panes))
    }

    fn control_tab_set_geometry(
        &mut self,
        id: String,
        handle: Option<&ControlConnectionHandle>,
        params: &TabSetGeometryParams,
        pane_sizes: Option<&std::collections::BTreeMap<String, api::schema::PaneTerminalSize>>,
    ) -> String {
        let connection_id = match self.control_connection_id(handle) {
            Ok(connection_id) => connection_id,
            Err(message) => return error(id, "control_stream_required", message),
        };
        if params.cols == 0 || params.rows == 0 {
            return error(
                id,
                "invalid_request",
                "tab.set_geometry cols and rows must be greater than 0".into(),
            );
        }
        if self.app.parse_tab_id(&params.tab_id).is_none() {
            return error(id, "not_found", format!("tab {} not found", params.tab_id));
        }
        let cell_size = crate::kitty_graphics::HostCellSize {
            width_px: params.cell_width_px,
            height_px: params.cell_height_px,
        };
        let owns = self.tab_geometry_controllers.get(&params.tab_id) == Some(&connection_id);
        let stamp = self.allocate_activity_stamp();
        let Some(state) = self.control_connections.get_mut(&connection_id) else {
            return error(
                id,
                "control_stream_required",
                "control stream is not open".into(),
            );
        };
        if let Some(sizes) = pane_sizes {
            state
                .tab_pane_geometry
                .insert(params.tab_id.clone(), sizes.clone());
        } else {
            state.tab_pane_geometry.remove(&params.tab_id);
        }
        let geometry =
            state
                .tab_geometry
                .entry(params.tab_id.clone())
                .or_insert(ControlTabGeometry {
                    cols: params.cols,
                    rows: params.rows,
                    cell_size,
                    chrome: params.chrome,
                    last_interaction: stamp,
                });
        geometry.cols = params.cols;
        geometry.rows = params.rows;
        geometry.cell_size = cell_size;
        geometry.chrome = params.chrome;
        if params.claim {
            geometry.last_interaction = stamp;
        }
        let geometry = *geometry;
        if params.claim || owns {
            self.claim_control_tab_geometry(connection_id, &params.tab_id, geometry);
        } else {
            // Stored only: the size applies when this stream claims later.
            self.sync_control_geometry_tabs();
        }
        success(id, ResponseResult::Ok {})
    }

    fn control_tab_claim_geometry(
        &mut self,
        id: String,
        handle: Option<&ControlConnectionHandle>,
        params: &TabClaimGeometryParams,
    ) -> String {
        let connection_id = match self.control_connection_id(handle) {
            Ok(connection_id) => connection_id,
            Err(message) => return error(id, "control_stream_required", message),
        };
        let tab_id = match (&params.tab_id, &params.attach_id) {
            (Some(tab_id), _) => tab_id.clone(),
            (None, Some(attach_id)) => {
                let terminal_id = self
                    .control_connections
                    .get(&connection_id)
                    .and_then(|state| state.attaches.get(attach_id))
                    .map(|attach| attach.terminal_id.clone());
                let Some(terminal_id) = terminal_id else {
                    return error(
                        id,
                        "unknown_attach",
                        format!("attach {attach_id} is not live"),
                    );
                };
                let Some(tab_id) = self
                    .tab_target_for_terminal(&terminal_id)
                    .and_then(|target| self.tab_id_for_target(target))
                else {
                    return error(
                        id,
                        "not_found",
                        format!("terminal {terminal_id} is not in a tab"),
                    );
                };
                tab_id
            }
            (None, None) => {
                return error(
                    id,
                    "invalid_request",
                    "tab.claim_geometry needs tab_id or attach_id".into(),
                );
            }
        };
        if self.app.parse_tab_id(&tab_id).is_none() {
            return error(id, "not_found", format!("tab {tab_id} not found"));
        }
        let stamp = self.allocate_activity_stamp();
        let Some(geometry) = self
            .control_connections
            .get_mut(&connection_id)
            .and_then(|state| state.tab_geometry.get_mut(&tab_id))
        else {
            return error(
                id,
                "no_geometry",
                format!("no size stored for tab {tab_id}; send tab.set_geometry first"),
            );
        };
        geometry.last_interaction = stamp;
        let geometry = *geometry;
        self.claim_control_tab_geometry(connection_id, &tab_id, geometry);
        success(id, ResponseResult::Ok {})
    }

    /// Makes a control connection the tab's geometry controller and lays
    /// the tab out at its stored size.
    fn claim_control_tab_geometry(
        &mut self,
        connection_id: u64,
        tab_id: &str,
        geometry: ControlTabGeometry,
    ) {
        let Some((workspace_index, tab_index)) = self.app.parse_tab_id(tab_id) else {
            return;
        };
        self.set_tab_geometry_controller(tab_id, Some(connection_id));
        self.apply_control_tab_geometry(
            crate::ui::TabSurfaceTarget {
                workspace_index,
                tab_index,
            },
            geometry.cols,
            geometry.rows,
            geometry.cell_size,
        );
    }

    /// The one write path for `tab_geometry_controllers`.
    pub(super) fn set_tab_geometry_controller(&mut self, tab_id: &str, controller: Option<u64>) {
        match controller {
            Some(controller) => {
                self.tab_geometry_controllers
                    .insert(tab_id.to_owned(), controller);
            }
            None => {
                self.tab_geometry_controllers.remove(tab_id);
            }
        }
        self.sync_control_geometry_tabs();
    }

    /// Drops stored sizes for tabs that no longer exist.
    pub(super) fn prune_control_tab_geometry(&mut self, keep: impl Fn(&str) -> bool) {
        for state in self.control_connections.values_mut() {
            state.tab_geometry.retain(|tab_id, _| keep(tab_id));
            state.tab_pane_geometry.retain(|tab_id, _| keep(tab_id));
        }
    }

    /// Resolves a terminal id string to its runtime through the pane path
    /// first, which also serves test workspaces; popup terminals fall back to
    /// the terminal registry.
    fn control_runtime(&self, terminal_id: &str) -> Option<&crate::terminal::TerminalRuntime> {
        if let Ok(target) = self.app.resolve_terminal_target(terminal_id) {
            if let Some(runtime) = self.app.state.runtime_for_pane_in_workspace(
                &self.app.terminal_runtimes,
                target.ws_idx,
                target.pane_id,
            ) {
                return Some(runtime);
            }
        }
        self.runtime_for_terminal_id_string(terminal_id)
    }

    pub(super) fn apply_control_tab_geometry(
        &mut self,
        target: crate::ui::TabSurfaceTarget,
        cols: u16,
        rows: u16,
        cell_size: crate::kitty_graphics::HostCellSize,
    ) {
        let area = Rect::new(0, 0, cols, rows);
        self.with_tab_layout_boundary(&[target], area, |server| {
            crate::ui::compute_tab_surface_for(
                &server.app.state,
                &server.app.terminal_runtimes,
                Some(target),
                area,
                true,
                cell_size,
            );
        });
        self.finish_shell_tab_geometry_change(true);
    }

    pub(super) fn all_tab_targets(&self) -> Vec<crate::ui::TabSurfaceTarget> {
        self.app
            .state
            .workspaces
            .iter()
            .enumerate()
            .flat_map(|(workspace_index, workspace)| {
                (0..workspace.tabs.len()).map(move |tab_index| crate::ui::TabSurfaceTarget {
                    workspace_index,
                    tab_index,
                })
            })
            .collect()
    }

    /// Lays `targets` out in `area` behind one boundary: every pane's
    /// content lock is held while the `tab.layout` records go out and
    /// `resize` runs, so no PTY read can publish old-size output after the
    /// record or new-size output before it. Shell-only servers just resize.
    pub(super) fn with_tab_layout_boundary(
        &self,
        targets: &[crate::ui::TabSurfaceTarget],
        area: Rect,
        resize: impl FnOnce(&Self),
    ) {
        if self.control_connections.is_empty() {
            resize(self);
            return;
        }
        let holds = targets
            .iter()
            .filter_map(|target| {
                let tab = self
                    .app
                    .state
                    .workspaces
                    .get(target.workspace_index)?
                    .tabs
                    .get(target.tab_index)?;
                Some((target.workspace_index, tab.layout.pane_ids()))
            })
            .flat_map(|(workspace_index, pane_ids)| {
                pane_ids.into_iter().filter_map(move |pane_id| {
                    self.app.state.runtime_for_pane_in_workspace(
                        &self.app.terminal_runtimes,
                        workspace_index,
                        pane_id,
                    )
                })
            })
            .map(|runtime| runtime.hold_content_write_lock())
            .collect::<Vec<_>>();
        for target in targets {
            self.push_control_tab_layout(*target, area);
        }
        resize(self);
        drop(holds);
    }

    /// Sends a `tab.layout` record for the tab about to be laid out in
    /// `area` to every control stream. The record travels the same ordered
    /// lane as raw output; `with_tab_layout_boundary` makes that order hold.
    fn push_control_tab_layout(&self, target: crate::ui::TabSurfaceTarget, area: Rect) {
        let Some(layout) = self.control_tab_layout(target, area) else {
            return;
        };
        // Serialize once per protocol, not once per attached viewer.
        let current = serde_json::to_string(&api::schema::ControlRecord::TabLayout {
            layout: layout.clone(),
        })
        .ok();
        let legacy = if self
            .control_connections
            .values()
            .any(|state| state.protocol < 3)
        {
            serde_json::to_string(&api::schema::ControlRecord::TabLayout {
                layout: self.layout_for_control_protocol(layout, 2),
            })
            .ok()
        } else {
            None
        };
        for state in self.control_connections.values() {
            if let Some(line) = if state.protocol >= 3 {
                &current
            } else {
                &legacy
            } {
                state.handle.send_line(line.clone());
            }
        }
    }

    /// Size a control connection asked for on a tab, when it controls it.
    pub(super) fn control_tab_geometry_for_target(
        &self,
        controller_id: u64,
        target: crate::ui::TabSurfaceTarget,
    ) -> Option<(u16, u16, crate::kitty_graphics::HostCellSize)> {
        if !is_control_connection_id(controller_id) {
            return None;
        }
        let tab_id = self.tab_id_for_target(target)?;
        self.control_connections
            .get(&controller_id)?
            .tab_geometry(&tab_id)
    }

    /// Mirrors `tab_geometry_controllers` into everything derived from it:
    /// the control-owned and chromeless tab sets for `crate::ui`, the
    /// controller each layout snapshot reports (announcing changes), the
    /// input-claim flags of every stream, and query authority on tabs that
    /// changed hands. Call after every mutation of `tab_geometry_controllers`.
    pub(super) fn sync_control_geometry_tabs(&mut self) {
        self.app.state.control_geometry_tabs = self
            .tab_geometry_controllers
            .iter()
            .filter(|(_, controller)| is_control_connection_id(**controller))
            .map(|(tab_id, _)| tab_id.clone())
            .collect();

        let mut controllers = HashMap::new();
        let mut chromeless = HashSet::new();
        for (tab_id, &controller) in &self.tab_geometry_controllers {
            let info = if is_control_connection_id(controller) {
                let chrome = self
                    .control_connections
                    .get(&controller)
                    .and_then(|state| state.tab_geometry.get(tab_id))
                    .map(|geometry| geometry.chrome)
                    .unwrap_or_default();
                if chrome == TabChrome::None {
                    chromeless.insert(tab_id.clone());
                }
                GeometryController {
                    kind: GeometryControllerKind::Control,
                    connection_id: Some(controller),
                    chrome,
                }
            } else {
                GeometryController {
                    kind: GeometryControllerKind::Client,
                    connection_id: Some(controller),
                    chrome: TabChrome::Server,
                }
            };
            controllers.insert(tab_id.clone(), info);
        }
        let mut pane_geometry = HashMap::new();
        for (tab_id, controller) in &self.tab_geometry_controllers {
            let Some(sizes) = self
                .control_connections
                .get(controller)
                .and_then(|state| state.tab_pane_geometry.get(tab_id))
            else {
                continue;
            };
            let Some((ws, tab)) = self.app.parse_tab_id(tab_id) else {
                continue;
            };
            let panes: HashSet<_> = self.app.state.workspaces[ws].tabs[tab]
                .layout
                .pane_ids()
                .into_iter()
                .collect();
            for (public_id, size) in sizes {
                if let Some((pane_ws, pane_id)) = self.app.parse_pane_id(public_id) {
                    if pane_ws == ws && panes.contains(&pane_id) {
                        pane_geometry.insert(pane_id, *size);
                    }
                }
            }
        }
        self.app.state.control_pane_geometry = pane_geometry;
        self.app.state.control_chromeless_tabs = chromeless;

        let previous = std::mem::replace(
            &mut self.app.state.control_tab_geometry_controllers,
            controllers.clone(),
        );
        let mut changed_tabs = Vec::new();
        for (tab_id, info) in &controllers {
            if previous.get(tab_id) != Some(info) {
                changed_tabs.push((tab_id.clone(), info.clone(), previous.get(tab_id).cloned()));
            }
        }
        for (tab_id, info) in &previous {
            if !controllers.contains_key(tab_id) {
                changed_tabs.push((
                    tab_id.clone(),
                    GeometryController::default(),
                    Some(info.clone()),
                ));
            }
        }
        changed_tabs.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        for (tab_id, info, previous) in &changed_tabs {
            self.app
                .emit_tab_geometry_changed(tab_id.clone(), info.clone(), previous.clone());
        }

        // Input claims: an attach whose tab this stream sized but does not
        // own takes the tab on the next keystroke.
        let attach_tabs = self
            .control_connections
            .iter()
            .flat_map(|(&connection_id, state)| {
                state.attaches.iter().map(move |(attach_id, attach)| {
                    (connection_id, attach_id.clone(), attach.terminal_id.clone())
                })
            })
            .collect::<Vec<_>>();
        let mut tab_by_terminal: HashMap<String, Option<String>> = HashMap::new();
        let mut flags: HashMap<u64, HashSet<String>> = HashMap::new();
        for (connection_id, attach_id, terminal_id) in &attach_tabs {
            let tab_id = tab_by_terminal
                .entry(terminal_id.clone())
                .or_insert_with(|| {
                    self.tab_target_for_terminal(terminal_id)
                        .and_then(|target| self.tab_id_for_target(target))
                })
                .clone();
            let Some(tab_id) = tab_id else {
                continue;
            };
            let stored = self
                .control_connections
                .get(connection_id)
                .is_some_and(|state| state.holds_tab(&tab_id));
            let owns = self.tab_geometry_controllers.get(&tab_id) == Some(connection_id);
            if stored && !owns {
                flags
                    .entry(*connection_id)
                    .or_default()
                    .insert(attach_id.clone());
            }
        }
        for (&connection_id, state) in &self.control_connections {
            state
                .handle
                .set_claim_on_input(flags.remove(&connection_id).unwrap_or_default());
        }

        if !changed_tabs.is_empty() {
            let changed = changed_tabs
                .iter()
                .map(|(tab_id, _, _)| tab_id.clone())
                .collect::<HashSet<_>>();
            let terminals = attach_tabs
                .iter()
                .filter(|(_, _, terminal_id)| {
                    tab_by_terminal
                        .get(terminal_id)
                        .and_then(|tab_id| tab_id.as_ref())
                        .is_some_and(|tab_id| changed.contains(tab_id))
                })
                .map(|(_, _, terminal_id)| terminal_id.clone())
                .collect::<HashSet<_>>();
            for terminal_id in terminals {
                self.sync_query_authority(&terminal_id);
            }
        }
    }

    pub(super) fn control_connection_holds_tab(&self, controller_id: u64, tab_id: &str) -> bool {
        self.control_connections
            .get(&controller_id)
            .is_some_and(|state| state.holds_tab(tab_id))
    }

    /// Control connections that currently own a tab's geometry, for the
    /// single-client fast path.
    pub(super) fn control_connections_with_tab_claims(&self) -> usize {
        self.tab_geometry_controllers
            .values()
            .filter(|controller| is_control_connection_id(**controller))
            .collect::<HashSet<_>>()
            .len()
    }

    pub(super) fn control_connection_ids(&self) -> impl Iterator<Item = u64> + '_ {
        self.control_connections.keys().copied()
    }
}
