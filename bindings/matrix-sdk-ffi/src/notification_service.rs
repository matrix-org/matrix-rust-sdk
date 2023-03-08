use std::sync::Arc;

use crate::TimelineItem;

#[allow(dead_code)]
pub struct NotificationService {
    base_path: String,
    user_id: String,
}

/// Notification item struct.
pub struct NotificationItem {
    /// Actual timeline item for the event sent.
    pub item: Arc<TimelineItem>,
    /// Title of the notification. Usually would be event sender's display name.
    pub title: String,
    /// Subtitle of the notification. Usually would be the room name for
    /// non-direct rooms, and none for direct rooms.
    pub subtitle: Option<String>,
    /// Flag indicating the notification should play a sound.
    pub is_noisy: bool,
    /// Avatar url of the room the event sent to (if any).
    pub avatar_url: Option<String>,
}

impl NotificationService {
    /// Creates a new notification service.
    ///
    /// Will be used to fetch an event after receiving a notification.
    /// Please note that this will be called on a new process than the
    /// application context.
    pub fn new(base_path: String, user_id: String) -> Self {
        Self { base_path, user_id }
    }

    /// Get notification item for a given `room_id `and `event_id`.
    ///
    /// Returns `None` if this notification should not be displayed to the user.
    pub fn get_notification_item(
        &self,
        _room_id: String,
        _event_id: String,
    ) -> anyhow::Result<Option<NotificationItem>> {
        // TODO: Implement
        Ok(None)
    }
}
