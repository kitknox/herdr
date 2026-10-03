//! Raw PTY output taps for control-stream attaches.
//!
//! A tap forwards every PTY read to one control connection. Taps are
//! published under the pane's content write lock, so a snapshot taken under
//! the same lock is exactly ordered against the output that follows it.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use bytes::Bytes;

use crate::api::control::{ControlOutbound, ControlOutboundSender};
use crate::api::schema::{TerminalDetachReason, TerminalSnapshot};

/// Bytes a single tap may have queued but not yet written to its socket.
pub(crate) const DEFAULT_TAP_BUDGET_BYTES: usize = 4 * 1024 * 1024;
/// Longest unfinished sequence carried across reads for snapshots; a
/// larger one (a graphics payload mid-flight) is abandoned instead.
const MAX_PENDING_TAIL_BYTES: usize = 256 * 1024;

/// Where control-stream input bytes go, bypassing the app loop.
#[derive(Clone)]
pub(crate) enum RawInputSink {
    Actor(crate::pty::actor::PtyIoActorHandle),
    #[cfg(test)]
    Channel(tokio::sync::mpsc::Sender<Bytes>),
}

impl RawInputSink {
    pub(crate) fn try_send(
        &self,
        bytes: Bytes,
    ) -> Result<(), tokio::sync::mpsc::error::TrySendError<Bytes>> {
        match self {
            Self::Actor(actor) => actor.try_write_user_input(bytes),
            #[cfg(test)]
            Self::Channel(sender) => sender.try_send(bytes),
        }
    }
}

/// Byte accounting shared by a tap (producer) and its writer thread (consumer).
#[derive(Debug)]
pub(crate) struct RawTapBudget {
    pending: AtomicUsize,
    limit: usize,
}

impl RawTapBudget {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            pending: AtomicUsize::new(0),
            limit,
        }
    }

    fn reserve(&self, len: usize) -> bool {
        let mut current = self.pending.load(Ordering::Acquire);
        loop {
            if current.saturating_add(len) > self.limit {
                return false;
            }
            match self.pending.compare_exchange_weak(
                current,
                current + len,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    pub(crate) fn release(&self, len: usize) {
        self.pending.fetch_sub(len, Ordering::AcqRel);
    }
}

struct RawTap {
    attach_id: String,
    outbound: ControlOutboundSender,
    budget: Arc<RawTapBudget>,
    seq: u64,
    /// Bytes dropped since the last gap record; zero when not in a gap.
    dropped: u64,
    /// False until the first snapshot went out: output before it would name
    /// an attach the client has not been told about, and the snapshot taken
    /// under the same lock covers those bytes anyway.
    armed: bool,
    /// The client wants to answer terminal queries.
    suppress_responses: bool,
    /// The one tap on this terminal chosen to answer queries; suppression
    /// follows it, not every client-authority tap. None until the first
    /// sync so every protocol 2 tap is told its standing once.
    is_authority: Option<bool>,
    /// Control protocol of the owning connection; authority records go
    /// only to protocol 2 and later.
    protocol: u32,
    /// History the client asked for with its last snapshot; snapshots the
    /// server starts on its own reuse it.
    history_limit_bytes: usize,
}

impl RawTap {
    fn publish(&mut self, bytes: &[u8]) {
        if !self.armed {
            return;
        }
        if !self.budget.reserve(bytes.len()) {
            let first_drop = self.dropped == 0;
            self.dropped += bytes.len() as u64;
            if first_drop {
                self.send_gap();
            }
            return;
        }
        if self.dropped > 0 {
            self.send_gap();
            self.dropped = 0;
        }
        self.seq += 1;
        let sent = self.outbound.send(ControlOutbound::Output {
            attach_id: self.attach_id.clone(),
            seq: self.seq,
            bytes: Bytes::copy_from_slice(bytes),
            budget: Arc::clone(&self.budget),
        });
        if sent.is_err() {
            self.budget.release(bytes.len());
        }
    }

    fn send_gap(&self) {
        let _ = self.outbound.send(ControlOutbound::Gap {
            attach_id: self.attach_id.clone(),
            seq: self.seq,
            dropped_bytes: self.dropped,
        });
    }

    fn send_snapshot(&self, snapshot: TerminalSnapshot) -> bool {
        self.outbound
            .send(ControlOutbound::Snapshot {
                attach_id: self.attach_id.clone(),
                snapshot: Box::new(snapshot),
            })
            .is_ok()
    }

    fn send_detached(&self, reason: TerminalDetachReason) {
        let _ = self.outbound.send(ControlOutbound::Detached {
            attach_id: self.attach_id.clone(),
            reason,
        });
    }
}

#[derive(Default)]
pub(crate) struct RawTapRegistry {
    count: AtomicUsize,
    taps: Mutex<Vec<RawTap>>,
    /// Set while an attach with client query authority owns the terminal.
    suppress_terminal_responses: AtomicBool,
    /// Bytes of an escape or UTF-8 sequence the last read ended inside.
    /// The parser holds them; screen state cannot, so a snapshot replays
    /// them ahead of the read that completes the sequence.
    pending_tail: Mutex<Vec<u8>>,
}

impl RawTapRegistry {
    pub(crate) fn has_taps(&self) -> bool {
        self.count.load(Ordering::Acquire) > 0
    }

    /// Track where the parser stands after a PTY read. Call with the
    /// content write lock held, for every read, taps or not.
    pub(crate) fn note_read(&self, bytes: &[u8]) {
        let Ok(mut pending) = self.pending_tail.lock() else {
            return;
        };
        if pending.is_empty() {
            match incomplete_tail_start(bytes) {
                Some(start) if bytes.len() - start <= MAX_PENDING_TAIL_BYTES => {
                    pending.extend_from_slice(&bytes[start..]);
                }
                _ => {}
            }
            return;
        }
        pending.extend_from_slice(bytes);
        match incomplete_tail_start(&pending) {
            Some(start) if pending.len() - start <= MAX_PENDING_TAIL_BYTES => {
                pending.drain(..start);
            }
            _ => pending.clear(),
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_tail_for_test(&self) -> Vec<u8> {
        self.pending_tail
            .lock()
            .map(|pending| pending.clone())
            .unwrap_or_default()
    }

    pub(crate) fn suppress_terminal_responses(&self) -> bool {
        self.suppress_terminal_responses.load(Ordering::Acquire)
    }

    /// Forward one PTY read to every tap. Call with the content write lock held.
    pub(crate) fn publish(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let Ok(mut taps) = self.taps.lock() else {
            return;
        };
        for tap in taps.iter_mut() {
            tap.publish(bytes);
        }
    }

    /// Register a tap. Output numbering starts at one; the first snapshot is
    /// requested separately so the attach response can precede it.
    /// Call with the content write lock held.
    pub(crate) fn insert(
        &self,
        attach_id: String,
        outbound: ControlOutboundSender,
        budget: Arc<RawTapBudget>,
        suppress_responses: bool,
        protocol: u32,
    ) {
        let tap = RawTap {
            attach_id,
            outbound,
            budget,
            seq: 0,
            dropped: 0,
            armed: false,
            suppress_responses,
            is_authority: None,
            protocol,
            history_limit_bytes: 0,
        };
        if let Ok(mut taps) = self.taps.lock() {
            taps.push(tap);
            self.count.store(taps.len(), Ordering::Release);
        }
    }

    /// Names the tap that answers queries. Taps whose standing changed and
    /// speak protocol 2 learn it through a `terminal.authority` record.
    pub(crate) fn set_authority(&self, attach_id: Option<&str>) {
        let Ok(mut taps) = self.taps.lock() else {
            return;
        };
        for tap in taps.iter_mut() {
            let is_authority = attach_id == Some(tap.attach_id.as_str());
            if tap.is_authority == Some(is_authority) {
                continue;
            }
            tap.is_authority = Some(is_authority);
            if tap.protocol >= 2 {
                let _ = tap.outbound.send(ControlOutbound::Authority {
                    attach_id: tap.attach_id.clone(),
                    answers_queries: is_authority && tap.suppress_responses,
                });
            }
        }
        self.sync_suppression(&taps);
    }

    #[cfg(test)]
    pub(crate) fn authority_for_test(&self) -> Option<String> {
        self.taps
            .lock()
            .ok()?
            .iter()
            .find_map(|tap| (tap.is_authority == Some(true)).then(|| tap.attach_id.clone()))
    }

    /// Emit a fresh snapshot on one tap, stamped with its current sequence.
    /// The first snapshot arms the tap and, with it, query suppression: a
    /// query the client never sees must still be answered here.
    /// Call with the content write lock held.
    pub(crate) fn send_snapshot(
        &self,
        attach_id: &str,
        history_limit_bytes: usize,
        snapshot: impl FnOnce(u64) -> Option<TerminalSnapshot>,
    ) -> bool {
        let Ok(mut taps) = self.taps.lock() else {
            return false;
        };
        let Some(tap) = taps.iter_mut().find(|tap| tap.attach_id == attach_id) else {
            return false;
        };
        tap.dropped = 0;
        tap.history_limit_bytes = history_limit_bytes;
        match snapshot(tap.seq) {
            Some(snapshot) => {
                tap.armed = true;
                let sent = tap.send_snapshot(snapshot);
                // The snapshot shows state before the unfinished sequence;
                // replay its bytes so the read that completes it parses whole.
                if let Ok(pending) = self.pending_tail.lock() {
                    if !pending.is_empty() {
                        tap.publish(&pending);
                    }
                }
                self.sync_suppression(&taps);
                sent
            }
            None => false,
        }
    }

    /// Emit a fresh snapshot on every armed tap, for screen changes no PTY
    /// output describes. Each uses the history its client last asked for.
    /// Call with the content write lock held.
    pub(crate) fn resnapshot_armed(
        &self,
        snapshot: impl Fn(u64, usize) -> Option<TerminalSnapshot>,
    ) {
        let Ok(mut taps) = self.taps.lock() else {
            return;
        };
        let pending = self
            .pending_tail
            .lock()
            .map(|pending| pending.clone())
            .unwrap_or_default();
        for tap in taps.iter_mut().filter(|tap| tap.armed) {
            let Some(snapshot) = snapshot(tap.seq, tap.history_limit_bytes) else {
                continue;
            };
            tap.dropped = 0;
            tap.send_snapshot(snapshot);
            if !pending.is_empty() {
                tap.publish(&pending);
            }
        }
    }

    pub(crate) fn remove(&self, attach_id: &str, reason: TerminalDetachReason) -> bool {
        let Ok(mut taps) = self.taps.lock() else {
            return false;
        };
        let Some(index) = taps.iter().position(|tap| tap.attach_id == attach_id) else {
            return false;
        };
        let tap = taps.remove(index);
        self.count.store(taps.len(), Ordering::Release);
        self.sync_suppression(&taps);
        tap.send_detached(reason);
        true
    }

    pub(crate) fn remove_all(&self, reason: TerminalDetachReason) {
        let Ok(mut taps) = self.taps.lock() else {
            return;
        };
        for tap in taps.drain(..) {
            tap.send_detached(reason);
        }
        self.count.store(0, Ordering::Release);
        self.sync_suppression(&taps);
    }

    fn sync_suppression(&self, taps: &[RawTap]) {
        let suppress = taps
            .iter()
            .any(|tap| tap.armed && tap.is_authority == Some(true) && tap.suppress_responses);
        self.suppress_terminal_responses
            .store(suppress, Ordering::Release);
    }
}

impl Drop for RawTapRegistry {
    fn drop(&mut self) {
        self.remove_all(TerminalDetachReason::Closed);
    }
}

/// Offset where a sequence left unfinished at the end of `bytes` starts:
/// an escape sequence without its final byte, a string sequence (OSC, DCS,
/// APC, PM, SOS) without its terminator, or a truncated UTF-8 character.
fn incomplete_tail_start(bytes: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        let Some(&kind) = bytes.get(i + 1) else {
            return Some(i);
        };
        match kind {
            b'[' => {
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7e).contains(&bytes[j]) {
                    j += 1;
                }
                if j >= bytes.len() {
                    return Some(i);
                }
                i = j + 1;
            }
            b']' | b'P' | b'_' | b'^' | b'X' => {
                let bel_terminates = kind == b']';
                let mut j = i + 2;
                loop {
                    let Some(&byte) = bytes.get(j) else {
                        return Some(i);
                    };
                    if bel_terminates && byte == 0x07 {
                        i = j + 1;
                        break;
                    }
                    if byte == 0x1b {
                        match bytes.get(j + 1) {
                            None => return Some(i),
                            Some(b'\\') => {
                                i = j + 2;
                                break;
                            }
                            Some(_) => {}
                        }
                    }
                    j += 1;
                }
            }
            0x20..=0x2f => {
                let mut j = i + 2;
                while j < bytes.len() && (0x20..=0x2f).contains(&bytes[j]) {
                    j += 1;
                }
                if j >= bytes.len() {
                    return Some(i);
                }
                i = j + 1;
            }
            _ => i += 2,
        }
    }
    incomplete_utf8_start(bytes)
}

fn incomplete_utf8_start(bytes: &[u8]) -> Option<usize> {
    for back in 1..=3.min(bytes.len()) {
        let index = bytes.len() - back;
        let byte = bytes[index];
        if byte & 0xc0 == 0x80 {
            continue;
        }
        let needed = match byte {
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => return None,
        };
        return (back < needed).then_some(index);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{TerminalCursorInfo, TerminalScreenKind, TerminalStateInfo};

    fn snapshot(seq: u64) -> TerminalSnapshot {
        TerminalSnapshot {
            seq,
            active_screen: TerminalScreenKind::Primary,
            primary: Some(String::new()),
            alternate: None,
            state_ansi: String::new(),
            pen_ansi: String::new(),
            cursor: TerminalCursorInfo {
                x: 0,
                y: 0,
                visible: true,
                shape: 0,
                pending_wrap: false,
                pending_wrap_cell: None,
            },
            state: TerminalStateInfo::default(),
            truncated: false,
        }
    }

    /// Sends the first snapshot, which is what arms a tap for output.
    fn arm(
        registry: &RawTapRegistry,
        attach_id: &str,
        rx: &std::sync::mpsc::Receiver<ControlOutbound>,
    ) {
        assert!(registry.send_snapshot(attach_id, 1 << 20, |seq| Some(snapshot(seq))));
        assert!(matches!(
            rx.try_recv(),
            Ok(ControlOutbound::Snapshot { .. })
        ));
    }

    fn output_seqs(rx: &std::sync::mpsc::Receiver<ControlOutbound>) -> Vec<u64> {
        rx.try_iter()
            .filter_map(|record| match record {
                ControlOutbound::Output { seq, .. } => Some(seq),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn taps_stay_silent_until_the_first_snapshot() {
        let registry = RawTapRegistry::default();
        let (tx, rx) = std::sync::mpsc::channel();
        registry.insert("a1".into(), tx, Arc::new(RawTapBudget::new(1024)), false, 1);
        registry.publish(b"before");
        assert!(rx.try_recv().is_err());
        arm(&registry, "a1", &rx);
        registry.publish(b"after");
        assert_eq!(output_seqs(&rx), vec![1]);
    }

    /// A query emitted before the snapshot never reaches the client, so the
    /// server must keep answering until the tap is armed.
    #[test]
    fn query_suppression_starts_with_the_first_snapshot_and_ends_on_detach() {
        let registry = RawTapRegistry::default();
        let (tx, rx) = std::sync::mpsc::channel();
        registry.insert("a1".into(), tx, Arc::new(RawTapBudget::new(1024)), true, 1);
        registry.set_authority(Some("a1"));
        assert!(!registry.suppress_terminal_responses());
        arm(&registry, "a1", &rx);
        assert!(registry.suppress_terminal_responses());

        let (observer_tx, observer_rx) = std::sync::mpsc::channel();
        registry.insert(
            "a2".into(),
            observer_tx,
            Arc::new(RawTapBudget::new(1024)),
            false,
            1,
        );
        arm(&registry, "a2", &observer_rx);
        assert!(registry.remove("a1", TerminalDetachReason::Takeover));
        registry.set_authority(Some("a2"));
        assert!(
            !registry.suppress_terminal_responses(),
            "an observer never suppresses replies"
        );
    }

    /// Two client-authority taps share a terminal; only the designated one
    /// suppresses, and a protocol 2 tap hears each change of standing.
    #[test]
    fn suppression_follows_the_authority_and_protocol_two_taps_are_told() {
        let registry = RawTapRegistry::default();
        let (v1_tx, v1_rx) = std::sync::mpsc::channel();
        let (v2_tx, v2_rx) = std::sync::mpsc::channel();
        registry.insert(
            "a1".into(),
            v1_tx,
            Arc::new(RawTapBudget::new(1024)),
            true,
            1,
        );
        registry.insert(
            "b1".into(),
            v2_tx,
            Arc::new(RawTapBudget::new(1024)),
            true,
            2,
        );
        arm(&registry, "a1", &v1_rx);
        arm(&registry, "b1", &v2_rx);
        assert!(
            !registry.suppress_terminal_responses(),
            "no authority yet: the server answers"
        );

        registry.set_authority(Some("b1"));
        assert!(registry.suppress_terminal_responses());
        assert_eq!(registry.authority_for_test().as_deref(), Some("b1"));
        assert!(matches!(
            v2_rx.try_recv(),
            Ok(ControlOutbound::Authority {
                answers_queries: true,
                ..
            })
        ));
        assert!(
            v1_rx.try_recv().is_err(),
            "a protocol 1 tap never receives authority records"
        );

        registry.set_authority(Some("a1"));
        assert!(registry.suppress_terminal_responses());
        assert!(matches!(
            v2_rx.try_recv(),
            Ok(ControlOutbound::Authority {
                answers_queries: false,
                ..
            })
        ));
        assert!(v1_rx.try_recv().is_err());

        registry.set_authority(None);
        assert!(!registry.suppress_terminal_responses());
    }

    #[test]
    fn incomplete_tails_are_detected() {
        assert_eq!(incomplete_tail_start(b"plain text"), None);
        assert_eq!(incomplete_tail_start(b"abc\x1b"), Some(3));
        assert_eq!(incomplete_tail_start(b"abc\x1b["), Some(3));
        assert_eq!(incomplete_tail_start(b"abc\x1b[6"), Some(3));
        assert_eq!(incomplete_tail_start(b"abc\x1b[6n"), None);
        assert_eq!(incomplete_tail_start(b"\x1b]0;title"), Some(0));
        assert_eq!(incomplete_tail_start(b"\x1b]0;title\x1b"), Some(0));
        assert_eq!(incomplete_tail_start(b"\x1b]0;title\x1b\\x"), None);
        assert_eq!(incomplete_tail_start(b"\x1b]0;title\x07"), None);
        assert_eq!(incomplete_tail_start(b"\x1bPq..."), Some(0));
        assert_eq!(incomplete_tail_start(b"\x1b(B\x1b("), Some(3));
        assert_eq!(incomplete_tail_start(b"\x1b(B"), None);
        assert_eq!(incomplete_tail_start(b"caf\xc3"), Some(3));
        assert_eq!(incomplete_tail_start(b"caf\xc3\xa9"), None);
        assert_eq!(incomplete_tail_start(b"\xf0\x9f\x98"), Some(0));
    }

    #[test]
    fn pending_tail_follows_reads_and_replays_after_the_snapshot() {
        let registry = RawTapRegistry::default();
        registry.note_read(b"hello\x1b[");
        assert_eq!(registry.pending_tail_for_test(), b"\x1b[");
        registry.note_read(b"3");
        assert_eq!(registry.pending_tail_for_test(), b"\x1b[3");

        let (tx, rx) = std::sync::mpsc::channel();
        registry.insert("a1".into(), tx, Arc::new(RawTapBudget::new(1024)), true, 1);
        arm(&registry, "a1", &rx);
        registry.note_read(b"1m");
        registry.publish(b"1m");
        let records = rx.try_iter().collect::<Vec<_>>();
        match (&records[0], &records[1]) {
            (
                ControlOutbound::Output {
                    seq: 1,
                    bytes: tail,
                    ..
                },
                ControlOutbound::Output {
                    seq: 2,
                    bytes: rest,
                    ..
                },
            ) => {
                assert_eq!(&tail[..], b"\x1b[3");
                assert_eq!(&rest[..], b"1m");
            }
            other => panic!("expected the tail then the read, got {other:?}"),
        }
        assert!(registry.pending_tail_for_test().is_empty());
    }

    #[test]
    fn taps_number_output_from_one() {
        let registry = RawTapRegistry::default();
        let (tx, rx) = std::sync::mpsc::channel();
        registry.insert("a1".into(), tx, Arc::new(RawTapBudget::new(1024)), false, 1);
        arm(&registry, "a1", &rx);
        registry.publish(b"one");
        registry.publish(b"two");

        assert_eq!(output_seqs(&rx), vec![1, 2]);
    }

    #[test]
    fn a_full_budget_drops_output_and_reports_one_gap_per_episode() {
        let registry = RawTapRegistry::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let budget = Arc::new(RawTapBudget::new(4));
        registry.insert("a1".into(), tx, Arc::clone(&budget), false, 1);
        arm(&registry, "a1", &rx);

        registry.publish(b"1234");
        registry.publish(b"5");
        registry.publish(b"6");
        let mut records = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(records.len(), 2, "one output and one gap");
        assert!(matches!(
            records.remove(1),
            ControlOutbound::Gap {
                seq: 1,
                dropped_bytes: 1,
                ..
            }
        ));

        budget.release(4);
        registry.publish(b"7");
        let records = rx.try_iter().collect::<Vec<_>>();
        assert!(matches!(
            records[0],
            ControlOutbound::Gap {
                dropped_bytes: 2,
                ..
            }
        ));
        assert!(matches!(records[1], ControlOutbound::Output { seq: 2, .. }));
    }

    #[test]
    fn removing_a_tap_sends_detached_and_stops_publishing() {
        let registry = RawTapRegistry::default();
        let (tx, rx) = std::sync::mpsc::channel();
        registry.insert("a1".into(), tx, Arc::new(RawTapBudget::new(1024)), false, 1);
        assert!(registry.has_taps());
        assert!(registry.remove("a1", TerminalDetachReason::Takeover));
        assert!(!registry.has_taps());
        registry.publish(b"ignored");
        let records = rx.try_iter().collect::<Vec<_>>();
        assert!(matches!(
            records.last(),
            Some(ControlOutbound::Detached {
                reason: TerminalDetachReason::Takeover,
                ..
            })
        ));
        assert!(output_seqs_from(&records).is_empty());
    }

    fn output_seqs_from(records: &[ControlOutbound]) -> Vec<u64> {
        records
            .iter()
            .filter_map(|record| match record {
                ControlOutbound::Output { seq, .. } => Some(*seq),
                _ => None,
            })
            .collect()
    }
}
