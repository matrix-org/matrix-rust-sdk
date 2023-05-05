use std::sync::Arc;

use matrix_sdk::room::Room;
use ruma::{
    api::client::push::get_notifications::v3::Notification, events::AnySyncTimelineEvent,
    push::Action, EventId,
};

use crate::event::TimelineEvent;

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
    pub(crate) async fn new_from_notification(
        notification: Notification,
        room: Room,
    ) -> anyhow::Result<Self> {
        let deserialized_event = notification.event.deserialize()?;
        Self::new(deserialized_event, room, notification.actions).await
    }

    pub(crate) async fn new_from_event_id(event_id: &str, room: Room) -> anyhow::Result<Self> {
        let event_id = EventId::parse(event_id)?;
        let ruma_event = room.event(&event_id).await?;

        let event: AnySyncTimelineEvent = ruma_event.event.deserialize()?.into();
        Self::new(event, room, ruma_event.push_actions).await
    }

    async fn new(
        event: AnySyncTimelineEvent,
        room: Room,
        actions: Vec<Action>,
    ) -> anyhow::Result<Self> {
        let sender = room.get_member(event.sender()).await?;
        let mut sender_display_name = None;
        let mut sender_avatar_url = None;
        if let Some(sender) = sender {
            sender_display_name = sender.display_name().map(|s| s.to_owned());
            sender_avatar_url = sender.avatar_url().map(|s| s.to_string());
        }

        let is_noisy = actions.iter().any(|a| a.sound().is_some());
        let item = Self {
            event: Arc::new(TimelineEvent(event)),
            room_id: room.room_id().to_string(),
            sender_display_name,
            sender_avatar_url,
            room_display_name: room.display_name().await?.to_string(),
            room_avatar_url: room.avatar_url().map(|s| s.to_string()),
            is_noisy,
            is_direct: room.is_direct().await?,
            is_encrypted: room.is_encrypted().await?,
        };
        Ok(item)
    }
}
