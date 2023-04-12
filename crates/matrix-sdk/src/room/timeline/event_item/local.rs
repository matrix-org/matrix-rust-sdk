use ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedTransactionId, TransactionId};

use super::EventSendState;

/// An item for an event that was created locally and not yet echoed back by
/// the homeserver.
#[derive(Debug, Clone)]
pub struct LocalEventTimelineItem {
    /// The send state of this local event.
    send_state: EventSendState,
    /// The transaction ID.
    transaction_id: OwnedTransactionId,
    /// The timestamp of the event.
    timestamp: MilliSecondsSinceUnixEpoch,
}

impl LocalEventTimelineItem {
    pub(in crate::room::timeline) fn new(
        send_state: EventSendState,
        transaction_id: OwnedTransactionId,
        timestamp: MilliSecondsSinceUnixEpoch,
    ) -> Self {
        Self { send_state, transaction_id, timestamp }
    }

    /// Get the event's send state.
    pub fn send_state(&self) -> &EventSendState {
        &self.send_state
    }

    /// Get the event ID of this item.
    ///
    /// Will be `Some` if and only if `send_state` is
    /// `EventSendState::Sent`.
    pub fn event_id(&self) -> Option<&EventId> {
        match &self.send_state {
            EventSendState::Sent { event_id } => Some(event_id),
            _ => None,
        }
    }

    /// Get the transaction ID of the event.
    pub fn transaction_id(&self) -> &TransactionId {
        &self.transaction_id
    }

    /// Get the timestamp when the event was created locally.
    pub fn timestamp(&self) -> MilliSecondsSinceUnixEpoch {
        self.timestamp
    }

    /// Clone the current event item, and update its `send_state`.
    pub(in crate::room::timeline) fn with_send_state(&self, send_state: EventSendState) -> Self {
        Self { send_state, ..self.clone() }
    }
}
