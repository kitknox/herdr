//! Shared state between a control stream's socket threads and the server.
//!
//! The socket side owns the reader and writer threads; the server side owns
//! terminal attaches and geometry claims. They meet through this handle.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::api::schema::{TerminalDetachReason, TerminalSnapshot};
use crate::pane::raw_stream::{RawInputSink, RawTapBudget};

/// Version of the control stream contract advertised in `ping` capabilities.
pub const CONTROL_STREAM_PROTOCOL: u32 = 3;

/// How long an attach that just lost query authority may still deliver an
/// automatic reply: a query answered across a hand-off must not vanish.
pub(crate) const AUTHORITY_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// One attach's direct PTY input sink and its standing for automatic replies.
#[derive(Clone)]
struct InputEntry {
    sink: RawInputSink,
    authority: bool,
    /// When authority was last taken away; None while held or never held.
    lost_at: Option<std::time::Instant>,
}

impl InputEntry {
    fn answers_queries(&self) -> bool {
        self.authority
            || self
                .lost_at
                .is_some_and(|lost| lost.elapsed() <= AUTHORITY_GRACE)
    }
}

/// Feature names advertised as `control_features`; clients gate on these.
pub const CONTROL_FEATURES: &[&str] = &[
    "shared_attach",
    "geometry_ownership",
    "geometry_controller",
    "control_list",
    "client_identity",
    "query_authority",
    "event_drain",
    "event_gap",
    "auto_input",
    "pane_geometry",
];

/// One line or record queued for a control stream's writer thread.
#[derive(Debug)]
pub(crate) enum ControlOutbound {
    /// A finished JSON line (response or subscription event).
    Line(String),
    Snapshot {
        attach_id: String,
        snapshot: Box<TerminalSnapshot>,
    },
    Output {
        attach_id: String,
        seq: u64,
        bytes: Bytes,
        budget: Arc<RawTapBudget>,
    },
    Gap {
        attach_id: String,
        seq: u64,
        dropped_bytes: u64,
    },
    Detached {
        attach_id: String,
        reason: TerminalDetachReason,
    },
    Authority {
        attach_id: String,
        answers_queries: bool,
    },
}

pub(crate) type ControlOutboundSender = std::sync::mpsc::Sender<ControlOutbound>;

pub(crate) struct ControlConnectionShared {
    /// Server-assigned id; zero until `control.open` is handled.
    id: AtomicU64,
    /// Shared with threads that must not keep the outbound channel open.
    alive: Arc<AtomicBool>,
    outbound: ControlOutboundSender,
    /// Direct PTY input sinks per attach id, filled by the server on attach so
    /// keystrokes never wait behind the app loop, each with whether that
    /// attach is its terminal's query authority (automatic replies from any
    /// other attach are dropped once its grace window passes).
    inputs: Mutex<HashMap<String, InputEntry>>,
    /// Negotiated control protocol; 1 until `control.open` says otherwise.
    protocol: AtomicU32,
    /// Attaches whose next input should claim the tab's geometry: the
    /// server keeps this current so input never waits on the app loop.
    claim_on_input: Mutex<HashSet<String>>,
}

#[derive(Clone)]
pub struct ControlConnectionHandle(Arc<ControlConnectionShared>);

impl ControlConnectionHandle {
    pub(crate) fn new(outbound: ControlOutboundSender) -> Self {
        Self(Arc::new(ControlConnectionShared {
            id: AtomicU64::new(0),
            alive: Arc::new(AtomicBool::new(true)),
            outbound,
            inputs: Mutex::new(HashMap::new()),
            protocol: AtomicU32::new(1),
            claim_on_input: Mutex::new(HashSet::new()),
        }))
    }

    pub(crate) fn protocol(&self) -> u32 {
        self.0.protocol.load(Ordering::Acquire)
    }

    pub(crate) fn set_protocol(&self, protocol: u32) {
        self.0.protocol.store(protocol, Ordering::Release);
    }

    pub(crate) fn set_claim_on_input(&self, attach_ids: HashSet<String>) {
        if let Ok(mut flags) = self.0.claim_on_input.lock() {
            *flags = attach_ids;
        }
    }

    /// Clears and returns the flag so one input triggers one claim.
    pub(crate) fn take_claim_on_input(&self, attach_id: &str) -> bool {
        self.0
            .claim_on_input
            .lock()
            .map(|mut flags| flags.remove(attach_id))
            .unwrap_or(false)
    }

    pub(crate) fn id(&self) -> u64 {
        self.0.id.load(Ordering::Acquire)
    }

    pub(crate) fn assign_id(&self, id: u64) {
        self.0.id.store(id, Ordering::Release);
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.0.alive.load(Ordering::Acquire)
    }

    /// Liveness flag without the outbound sender, for the writer thread.
    pub(crate) fn alive_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.0.alive)
    }

    pub(crate) fn close(&self) {
        self.0.alive.store(false, Ordering::Release);
    }

    pub(crate) fn outbound(&self) -> ControlOutboundSender {
        self.0.outbound.clone()
    }

    pub(crate) fn send_line(&self, line: String) -> bool {
        self.0.outbound.send(ControlOutbound::Line(line)).is_ok()
    }

    pub(crate) fn register_input(&self, attach_id: String, sink: RawInputSink) {
        if let Ok(mut inputs) = self.0.inputs.lock() {
            inputs.insert(
                attach_id,
                InputEntry {
                    sink,
                    authority: false,
                    lost_at: None,
                },
            );
        }
    }

    pub(crate) fn unregister_input(&self, attach_id: &str) {
        if let Ok(mut inputs) = self.0.inputs.lock() {
            inputs.remove(attach_id);
        }
    }

    /// The server keeps this current from `sync_query_authority`.
    pub(crate) fn set_input_authority(&self, attach_id: &str, is_authority: bool) {
        if let Ok(mut inputs) = self.0.inputs.lock() {
            if let Some(entry) = inputs.get_mut(attach_id) {
                if entry.authority && !is_authority {
                    entry.lost_at = Some(std::time::Instant::now());
                } else if is_authority {
                    entry.lost_at = None;
                }
                entry.authority = is_authority;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn input_sink(&self, attach_id: &str) -> Option<RawInputSink> {
        self.input_sink_with_authority(attach_id)
            .map(|(sink, _)| sink)
    }

    /// The sink and whether the attach may deliver an automatic reply right
    /// now (it holds authority, or lost it within the grace window), under
    /// one lock.
    pub(crate) fn input_sink_with_authority(
        &self,
        attach_id: &str,
    ) -> Option<(RawInputSink, bool)> {
        let inputs = self.0.inputs.lock().ok()?;
        let entry = inputs.get(attach_id)?;
        Some((entry.sink.clone(), entry.answers_queries()))
    }

    /// The strict flag, without the grace window.
    #[cfg(test)]
    pub(crate) fn input_authority_for_test(&self, attach_id: &str) -> bool {
        self.0
            .inputs
            .lock()
            .ok()
            .and_then(|inputs| inputs.get(attach_id).map(|entry| entry.authority))
            .unwrap_or(false)
    }

    /// Moves the attach's authority loss `by` into the past.
    #[cfg(test)]
    pub(crate) fn backdate_authority_loss_for_test(
        &self,
        attach_id: &str,
        by: std::time::Duration,
    ) {
        if let Ok(mut inputs) = self.0.inputs.lock() {
            if let Some(entry) = inputs.get_mut(attach_id) {
                entry.lost_at = entry.lost_at.and_then(|lost| lost.checked_sub(by));
            }
        }
    }
}
