use std::sync::Arc;

use matrix_sdk::room::Room;
use ruma::api::client::push::get_notifications::v3::Notification;

use crate::event::TimelineEvent;

#[allow(dead_code)]
pub struct NotificationService {
    base_path: String,
    user_id: String,
}
pub struct NotificationItem {
    pub event: Arc<TimelineEvent>,
    pub room_id: String,

    pub sender_display_name: Option<String>,
    pub sender_avatar_url: Option<String>,

    pub room_display_name: String,
    pub room_avatar_url: Option<String>,

    pub is_noisy: bool,
    pub is_direct: bool,
    pub is_encrypted: bool,
}

impl NotificationItem {
    pub(crate) async fn new(notification: Notification, room: Room) -> anyhow::Result<Self> {
        let deserialized_event = notification.event.deserialize()?;
        let event_id = deserialized_event.event_id().to_string();

        let sender = room.get_member(deserialized_event.sender()).await?;
        let mut sender_display_name = None;
        let mut sender_avatar_url = None;
        if let Some(sender) = sender {
            sender_display_name = sender.display_name().map(|s| s.to_owned());
            sender_avatar_url = sender.avatar_url().map(|s| s.to_string());
        }

        let is_noisy =
            notification.actions.iter().any(|a| a.sound().is_some() && a.should_notify());

        let item = Self {
            event: Arc::new(TimelineEvent(deserialized_event)),
            room_id: room.room_id().to_string(),
            sender_display_name,
            sender_avatar_url,
            room_display_name: room.display_name().await?.to_string(),
            room_avatar_url: room.avatar_url().map(|s| s.to_string()),
            is_noisy,
            is_direct: room.is_direct(),
            is_encrypted: room.is_encrypted().await?,
        };
        Ok(item)
    }
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
