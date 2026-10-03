#[derive(Clone, Default)]
pub struct EventHub {
    inner: std::sync::Arc<std::sync::Mutex<EventHubState>>,
}

#[derive(Default)]
struct EventHubState {
    next_sequence: u64,
    events: Vec<(u64, crate::api::schema::EventEnvelope)>,
}

/// Events a reader missed because the ring dropped them before it polled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventGap {
    pub dropped: u64,
    /// Sequence of the oldest event still available.
    pub resume_sequence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum EventHistoryError {
    Lost,
    Unavailable,
}

impl EventHub {
    pub(crate) const MAX_EVENTS: usize = 2048;

    pub fn push(&self, event: crate::api::schema::EventEnvelope) {
        let Ok(mut state) = self.inner.lock() else {
            return;
        };
        state.next_sequence += 1;
        let sequence = state.next_sequence;
        state.events.push((sequence, event));
        let overflow = state.events.len().saturating_sub(Self::MAX_EVENTS);
        if overflow > 0 {
            state.events.drain(0..overflow);
        }
    }

    pub fn events_after(&self, sequence: u64) -> Vec<(u64, crate::api::schema::EventEnvelope)> {
        self.events_after_with_gap(sequence).0
    }

    /// Events after `sequence`, plus the gap when the ring no longer holds
    /// the ones right after it.
    pub fn events_after_with_gap(
        &self,
        sequence: u64,
    ) -> (
        Vec<(u64, crate::api::schema::EventEnvelope)>,
        Option<EventGap>,
    ) {
        let Ok(state) = self.inner.lock() else {
            return (Vec::new(), None);
        };
        let events = state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect();
        let gap = state
            .events
            .first()
            .map(|(oldest, _)| *oldest)
            .filter(|oldest| *oldest > sequence + 1)
            .map(|oldest| EventGap {
                dropped: oldest - sequence - 1,
                resume_sequence: oldest,
            });
        (events, gap)
    }

    pub(super) fn events_after_checked(
        &self,
        sequence: u64,
    ) -> Result<Vec<(u64, crate::api::schema::EventEnvelope)>, EventHistoryError> {
        let state = self
            .inner
            .lock()
            .map_err(|_| EventHistoryError::Unavailable)?;
        if state
            .events
            .first()
            .is_some_and(|(first, _)| sequence < first.saturating_sub(1))
        {
            return Err(EventHistoryError::Lost);
        }
        Ok(state
            .events
            .iter()
            .filter(|(event_sequence, _)| *event_sequence > sequence)
            .cloned()
            .collect())
    }

    pub fn current_sequence(&self) -> u64 {
        let Ok(state) = self.inner.lock() else {
            return 0;
        };
        state.next_sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{EventData, EventEnvelope, EventKind};

    fn event(index: u64) -> EventEnvelope {
        EventEnvelope {
            event: EventKind::TabFocused,
            data: EventData::TabFocused {
                tab_id: format!("w1:t{index}"),
                workspace_id: "w1".into(),
            },
        }
    }

    #[test]
    fn a_reader_within_the_ring_sees_no_gap() {
        let hub = EventHub::default();
        for index in 0..10 {
            hub.push(event(index));
        }
        let (events, gap) = hub.events_after_with_gap(4);
        assert_eq!(events.len(), 6);
        assert_eq!(gap, None);
        let (events, gap) = hub.events_after_with_gap(10);
        assert!(events.is_empty());
        assert_eq!(gap, None);
    }

    #[test]
    fn a_reader_behind_the_ring_learns_how_much_it_missed() {
        let hub = EventHub::default();
        let total = EventHub::MAX_EVENTS as u64 + 100;
        for index in 0..total {
            hub.push(event(index));
        }
        let oldest = total - EventHub::MAX_EVENTS as u64 + 1;
        let (events, gap) = hub.events_after_with_gap(0);
        assert_eq!(events.len(), EventHub::MAX_EVENTS);
        assert_eq!(
            gap,
            Some(EventGap {
                dropped: oldest - 1,
                resume_sequence: oldest,
            })
        );
        let (_, gap) = hub.events_after_with_gap(oldest - 1);
        assert_eq!(gap, None, "the next event is still retained");
    }

    #[test]
    fn checked_history_distinguishes_retained_boundary_from_lost_events() {
        let hub = EventHub::default();
        assert!(hub.events_after_checked(0).unwrap().is_empty());
        for _ in 0..EventHub::MAX_EVENTS {
            hub.push(event(0));
        }
        assert_eq!(
            hub.events_after_checked(0).unwrap().len(),
            EventHub::MAX_EVENTS
        );
        hub.push(event(0));
        assert_eq!(hub.events_after_checked(0), Err(EventHistoryError::Lost));
        let retained = hub.events_after_checked(1).unwrap();
        assert_eq!(retained.len(), EventHub::MAX_EVENTS);
        assert_eq!(retained.first().unwrap().0, 2);
        assert_eq!(retained.last().unwrap().0, hub.current_sequence());
        assert!(hub
            .events_after_checked(hub.current_sequence())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn checked_history_reports_unavailable_instead_of_empty_after_poison() {
        let hub = EventHub::default();
        assert!(std::panic::catch_unwind(|| {
            let _guard = hub.inner.lock().unwrap();
            panic!("poison the test event history");
        })
        .is_err());
        assert_eq!(
            hub.events_after_checked(0),
            Err(EventHistoryError::Unavailable)
        );
    }
}
