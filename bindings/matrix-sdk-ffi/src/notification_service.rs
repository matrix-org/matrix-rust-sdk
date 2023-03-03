use std::sync::Arc;

use matrix_sdk::{ruma::EventId, Client};
use ruma::{api::client::room, RoomId};

use super::RUNTIME;
use crate::TimelineItem;

#[allow(dead_code)]
pub struct NotificationService {
    client: Client,
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
    /// Will be used to fetch an event after receiving a notification.
    /// Please note that this will be called on a new process than the
    /// application context.
    pub fn new(client: Client, base_path: String, user_id: String) -> Self {
        NotificationService { client, base_path, user_id }
    }

    /// Get notification item for a given room_id and event_id.
    /// Returns none if this notification should not be displayed to the user.
    pub fn get_notification_item(
        &self,
        room_id: String,
        event_id: String,
    ) -> anyhow::Result<Option<NotificationItem>> {
        let room_id = RoomId::parse(room_id)?;
        let event_id = EventId::parse(event_id)?;
        let request = room::get_room_event::v3::Request::new(room_id, event_id);
        let response = RUNTIME.block_on(self.client.send(request, Default::default()))?;
        let item = NotificationItem {
            item: Arc::new(TimelineItem(matrix_sdk::room::timeline::TimelineItem::Event(
                timeline_event.into(),
            ))),
            title: "ismailgulek".to_owned(),
            subtitle: Some("Element iOS - Internal".to_owned()),
            is_noisy: true,
            avatar_url: Some("mxc://matrix.org/XzNaquIfpvjHtftUJBCRNDgX".to_owned()),
        };
        Ok(Some(item))
    }
}
