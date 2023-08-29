use ruma::events::{MessageLikeEventType, StateEventType};

/// Different kinds of filters for timeline events.
#[derive(Clone, Debug)]
pub enum EventFilter {
    /// Filter for message-like events.
    MessageLike(MessageLikeEventFilter),
    /// Filter for state events.
    State(StateEventFilter),
}

/// Filter for message-like events.
#[derive(Clone, Debug)]
pub enum MessageLikeEventFilter {
    /// Matches message-like events with the given `type`.
    WithType(MessageLikeEventType),
    /// Matches `m.room.message` events with the given `msgtype`.
    RoomMessageWithMsgtype(String),
}

/// Filter for state events.
#[derive(Clone, Debug)]
pub enum StateEventFilter {
    /// Matches state events with the given `type`, regardless of `state_key`.
    WithType(StateEventType),
    /// Matches state events with the given `type` and `state_key`.
    WithTypeAndStateKey(StateEventType, String),
}
