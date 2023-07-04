use std::sync::Arc;

use matrix_sdk::room::Room;
use ruma::{
    api::client::push::get_notifications::v3::Notification, events::AnySyncTimelineEvent,
    push::Action, EventId,
};

use crate::event::TimelineEvent;

#[derive(uniffi::Record)]
pub struct NotificationSenderInfo {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

#[derive(uniffi::Record)]
pub struct NotificationRoomInfo {
    pub id: String,
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub canonical_alias: Option<String>,
    pub joined_members_count: u64,
    pub is_encrypted: Option<bool>,
    pub is_direct: bool,
}

#[derive(uniffi::Record)]
pub struct NotificationItem {
    pub event: Arc<TimelineEvent>,

    pub sender_info: NotificationSenderInfo,
    pub room_info: NotificationRoomInfo,

    pub is_noisy: bool,
}

impl NotificationItem {
    pub(crate) async fn new_from_notification(
        notification: Notification,
        room: Room,
    ) -> anyhow::Result<Option<Self>> {
        if !notification.actions.iter().any(|a| a.should_notify()) {
            return Ok(None);
        }

        let deserialized_event = notification.event.deserialize()?;

        Self::new(deserialized_event, room, notification.actions).await.map(Some)
    }

    pub(crate) async fn new_from_event_id(
        event_id: &str,
        room: Room,
        filter_by_push_rules: bool,
    ) -> anyhow::Result<Option<Self>> {
        let event_id = EventId::parse(event_id)?;
        let ruma_event = room.event(&event_id).await?;

        if filter_by_push_rules && !ruma_event.push_actions.iter().any(|a| a.should_notify()) {
            return Ok(None);
        }

        let event: AnySyncTimelineEvent = ruma_event.event.deserialize()?.into();
        Self::new(event, room, ruma_event.push_actions).await.map(Some)
    }

    async fn new(
        event: AnySyncTimelineEvent,
        room: Room,
        actions: Vec<Action>,
    ) -> anyhow::Result<Self> {
        let sender = match &room {
            Room::Invited(invited) => invited.invite_details().await?.inviter,
            _ => room.get_member(event.sender()).await?,
        };
        let mut sender_display_name = None;
        let mut sender_avatar_url = None;
        if let Some(sender) = sender {
            sender_display_name = sender.display_name().map(|s| s.to_owned());
            sender_avatar_url = sender.avatar_url().map(|s| s.to_string());
        }

        let is_noisy = actions.iter().any(|a| a.sound().is_some());

        let room_info = NotificationRoomInfo {
            id: room.room_id().to_string(),
            display_name: room.display_name().await?.to_string(),
            avatar_url: room.avatar_url().map(|s| s.to_string()),
            canonical_alias: room.canonical_alias().map(|c| c.to_string()),
            joined_members_count: room.joined_members_count(),
            is_encrypted: room.is_encrypted().await.ok(),
            is_direct: room.is_direct().await?,
        };

        let sender_info = NotificationSenderInfo {
            display_name: sender_display_name,
            avatar_url: sender_avatar_url,
        };

        let item = Self { event: Arc::new(TimelineEvent(event)), sender_info, room_info, is_noisy };
        Ok(item)
    }
}
