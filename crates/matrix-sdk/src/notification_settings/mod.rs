//! High-level push notification settings API

mod rules;

/// Enum representing the push notification modes for a room.
#[derive(Debug, Clone, PartialEq)]
pub enum RoomNotificationMode {
    /// Receive notifications for all messages.
    AllMessages,
    /// Receive notifications for mentions and keywords only.
    MentionsAndKeywordsOnly,
    /// Do not receive any notifications.
    Mute,
}
