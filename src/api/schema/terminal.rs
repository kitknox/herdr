//! Control stream and raw terminal attach types.
//!
//! A control stream is one long-lived socket connection that multiplexes
//! ordinary requests, event subscriptions, and raw terminal output for a
//! client that renders panes with its own terminal emulator.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct ControlOpenParams {
    /// Who is opening the stream. Absent for protocol 1 clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ControlClientInfo>,
}

/// Identity a control client declares on `control.open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct ControlClientInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
    /// Highest control stream protocol the client speaks; the server
    /// negotiates down to what it supports.
    #[serde(default)]
    pub protocol: u32,
}

/// `tab.claim_geometry`: take a tab's geometry with the size this stream
/// stored for it. `attach_id` names the tab through an attach instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct TabClaimGeometryParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attach_id: Option<String>,
}

/// One open control stream, as `control.list` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ControlConnectionInfo {
    pub connection_id: u64,
    pub control_protocol: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<ControlClientInfo>,
    pub attaches: Vec<ControlAttachInfo>,
    pub tabs: Vec<ControlTabInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ControlAttachInfo {
    pub attach_id: String,
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    pub geometry: TerminalAttachGeometry,
    pub answer_queries: TerminalQueryAuthority,
    /// Whether this attach is the one emulator answering the pane's queries.
    pub answers_queries: bool,
}

/// A tab a control stream stored a size for. `controller` is true while the
/// stream owns the tab's geometry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ControlTabInfo {
    pub tab_id: String,
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub chrome: TabChrome,
    pub controller: bool,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAttachMode {
    /// Stream the pane's raw PTY output bytes.
    #[default]
    Raw,
}

/// Which terminal emulator answers queries the pane application sends
/// (device attributes, XTGETTCAP, OSC color queries).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum TerminalQueryAuthority {
    /// The attaching client answers; Herdr suppresses its own replies while
    /// the attach owns the terminal.
    #[default]
    Client,
    /// Herdr keeps answering as it does for every other client.
    Server,
}

/// Who decides the pane's size while the attach owns the terminal.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAttachGeometry {
    /// Sizes follow the tab layout; the client drives it with `tab.set_geometry`.
    #[default]
    Tab,
    /// The client sizes this one terminal with `terminal.resize` and Herdr
    /// locks the tab layout out of it, like a direct terminal attach.
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalAttachParams {
    /// Pane id, terminal id, or agent name.
    pub target: String,
    #[serde(default)]
    pub mode: TerminalAttachMode,
    /// Upper bound on primary-screen history bytes in the snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_limit_bytes: Option<u64>,
    #[serde(default)]
    pub answer_queries: TerminalQueryAuthority,
    #[serde(default)]
    pub takeover: bool,
    #[serde(default)]
    pub geometry: TerminalAttachGeometry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(default)]
    pub cell_width_px: u32,
    #[serde(default)]
    pub cell_height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalAttachTarget {
    pub attach_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalInputParams {
    pub attach_id: String,
    /// Base64 bytes written to the PTY verbatim.
    pub bytes: String,
    /// The client's emulator answered a query (DA, CPR, colours) rather
    /// than the user typing: forwarded only from the query authority and
    /// never counted as interaction.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalResizeParams {
    pub attach_id: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub cell_width_px: u32,
    #[serde(default)]
    pub cell_height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabSetGeometryParams {
    pub tab_id: String,
    pub cols: u16,
    pub rows: u16,
    #[serde(default)]
    pub cell_width_px: u32,
    #[serde(default)]
    pub cell_height_px: u32,
    /// Whether Herdr lays the tab out with its own borders, gaps, and
    /// scrollbar gutters, or hands the client bare pane rectangles.
    #[serde(default)]
    pub chrome: TabChrome,
    /// Take the tab's geometry now (the default). With `false` the size is
    /// only stored, for a later `tab.claim_geometry` or input-driven claim.
    #[serde(default = "default_true")]
    pub claim: bool,
}

/// Independent terminal grids, negotiated separately from tab layout coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct PaneTerminalSize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TabSetPaneGeometryParams {
    #[serde(flatten)]
    pub geometry: TabSetGeometryParams,
    pub panes: std::collections::BTreeMap<String, PaneTerminalSize>,
}

fn default_true() -> bool {
    true
}

/// Layout chrome for a tab a control stream sizes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TabChrome {
    /// Borders, gaps, and scrollbar gutters follow the server's config.
    #[default]
    Server,
    /// Panes tile the area exactly; the client draws its own dividers.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalScreenKind {
    Primary,
    Alternate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalCursorInfo {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
    /// DECSCUSR parameter (0 to 6).
    pub shape: u8,
    /// The next printable wraps to the next row: the cursor sits on the
    /// last column after a print. A client's cursor-position command
    /// clears that, so it has to be re-established.
    #[serde(default)]
    pub pending_wrap: bool,
    /// The cell under the cursor as styled VT, present with `pending_wrap`,
    /// so reprinting it sets the flag again without changing the screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_wrap_cell: Option<String>,
}

/// Terminal facts captured with the snapshot. Everything here is also
/// encoded in `state_ansi`; the fields exist so a client can read them
/// without parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema, Default)]
pub struct TerminalStateInfo {
    pub cols: u16,
    pub rows: u16,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
    pub focus_reporting: bool,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    pub mouse_alternate_scroll: bool,
    pub synchronized_output: bool,
    pub color_scheme_reporting: bool,
    pub kitty_keyboard_flags: u16,
    pub modify_other_keys_level: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scroll: Option<super::panes::PaneScrollInfo>,
}

/// Atomic capture of a terminal taken at attach or on request. Output
/// records with `seq` greater than this snapshot's `seq` were produced
/// after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct TerminalSnapshot {
    pub seq: u64,
    pub active_screen: TerminalScreenKind,
    /// Primary screen scrollback plus screen as unwrapped ANSI. Absent when
    /// the alternate screen is active, because only the active screen can
    /// be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    /// Alternate screen contents as ANSI when it is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alternate: Option<String>,
    /// Mode and keyboard-protocol sequences that restore terminal state.
    pub state_ansi: String,
    /// The active pen alone (SGR, hyperlink, protection), for restoring
    /// it after the client prints anything of its own.
    #[serde(default)]
    pub pen_ansi: String,
    pub cursor: TerminalCursorInfo,
    pub state: TerminalStateInfo,
    /// True when `history_limit_bytes` cut older history.
    pub truncated: bool,
}

/// Records pushed on a control stream outside request/response pairs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type")]
pub enum ControlRecord {
    #[serde(rename = "terminal.snapshot")]
    Snapshot {
        attach_id: String,
        snapshot: TerminalSnapshot,
    },
    #[serde(rename = "terminal.output")]
    Output {
        attach_id: String,
        seq: u64,
        /// Base64 raw PTY bytes.
        bytes: String,
    },
    /// Output was dropped because the client fell behind; request
    /// `terminal.snapshot` to resynchronize.
    #[serde(rename = "terminal.gap")]
    Gap {
        attach_id: String,
        seq: u64,
        dropped_bytes: u64,
    },
    #[serde(rename = "terminal.detached")]
    Detached {
        attach_id: String,
        reason: TerminalDetachReason,
    },
    /// A tab's pane rectangles as the terminals were (or are about to be)
    /// sized, ordered on the stream ahead of any output drawn at the new
    /// size. Sent for every tab geometry change while a control stream is
    /// open, whichever client caused it.
    #[serde(rename = "tab.layout")]
    TabLayout {
        layout: super::panes::PaneLayoutSnapshot,
    },
    /// Whether this attach now answers the pane's terminal queries. Sent
    /// only on control protocol 2 and later.
    #[serde(rename = "terminal.authority")]
    Authority {
        attach_id: String,
        answers_queries: bool,
    },
    /// Subscription events were lost because the stream fell behind; the
    /// next event delivered has sequence `resume_sequence`. Protocol 2+.
    #[serde(rename = "events.gap")]
    EventsGap { dropped: u64, resume_sequence: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalDetachReason {
    Takeover,
    Closed,
}
