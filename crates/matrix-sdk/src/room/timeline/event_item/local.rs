use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedTransactionId, OwnedUserId, TransactionId, UserId,
};

use super::{EventSendState, Profile, TimelineDetails, TimelineItemContent};

/// An item for an event that was created locally and not yet echoed back by
/// the homeserver.
#[derive(Debug, Clone)]
pub struct LocalEventTimelineItem {
    /// The send state of this local event.
    send_state: EventSendState,
    /// The transaction ID.
    transaction_id: OwnedTransactionId,
    /// The sender of the event.
    sender: OwnedUserId,
    /// The sender's profile of the event.
    sender_profile: TimelineDetails<Profile>,
    /// The timestamp of the event.
    timestamp: MilliSecondsSinceUnixEpoch,
    /// The content of the event.
    content: TimelineItemContent,
}

impl LocalEventTimelineItem {
    pub(in crate::room::timeline) fn new(
        send_state: EventSendState,
        transaction_id: OwnedTransactionId,
        sender: OwnedUserId,
        sender_profile: TimelineDetails<Profile>,
        timestamp: MilliSecondsSinceUnixEpoch,
        content: TimelineItemContent,
    ) -> Self {
        Self { send_state, transaction_id, sender, sender_profile, timestamp, content }
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

    /// Get the sender of the event.
    ///
    /// This is always the user's own user ID.
    pub(crate) fn sender(&self) -> &UserId {
        &self.sender
    }

    /// Get the profile of the event's sender.
    ///
    /// Since `LocalEventTimelineItem`s are always sent by the user that is
    /// logged in with the client that created the timeline, this effectively
    /// gives the sender's own (possibly room-specific) profile.
    pub fn sender_profile(&self) -> &TimelineDetails<Profile> {
        &self.sender_profile
    }

    /// Get the timestamp when the event was created locally.
    pub fn timestamp(&self) -> MilliSecondsSinceUnixEpoch {
        self.timestamp
    }

    /// Get the content of the event.
    pub fn content(&self) -> &TimelineItemContent {
        &self.content
    }

    /// Clone the current event item, and update its `send_state`.
    pub(in crate::room::timeline) fn with_send_state(&self, send_state: EventSendState) -> Self {
        Self { send_state, ..self.clone() }
    }

    /// Clone the current event item, and update its `sender_profile`.
    pub(in crate::room::timeline) fn with_sender_profile(
        &self,
        sender_profile: TimelineDetails<Profile>,
    ) -> Self {
        Self { sender_profile, ..self.clone() }
    }

    /// Clone the current event item, and update its `content`.
    pub(in crate::room::timeline) fn with_content(&self, content: TimelineItemContent) -> Self {
        Self { content, ..self.clone() }
    }
}
