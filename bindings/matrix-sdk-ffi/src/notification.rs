use std::sync::Arc;

use matrix_sdk::room::Room;
use ruma::{events::AnySyncTimelineEvent, EventId};

use crate::event::TimelineEvent;

#[derive(uniffi::Record)]
pub struct NotificationItem {
    pub event: Arc<TimelineEvent>,
    pub room_id: String,

    pub sender_display_name: Option<String>,
    pub sender_avatar_url: Option<String>,

    pub room_display_name: String,
    pub room_avatar_url: Option<String>,
    pub room_canonical_alias: Option<String>,

    pub is_noisy: bool,
    pub is_direct: bool,
    pub is_encrypted: Option<bool>,
}

impl NotificationItem {
    pub(crate) async fn new_from_event_id(event_id: &str, room: Room) -> anyhow::Result<Self> {
        let event_id = EventId::parse(event_id)?;
        let ruma_event = room.event(&event_id).await?;

        let event: AnySyncTimelineEvent = ruma_event.event.deserialize()?.into();

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

        let is_noisy = ruma_event.push_actions.iter().any(|a| a.sound().is_some());
        let item = Self {
            event: Arc::new(TimelineEvent(event)),
            room_id: room.room_id().to_string(),
            sender_display_name,
            sender_avatar_url,
            room_display_name: room.display_name().await?.to_string(),
            room_avatar_url: room.avatar_url().map(|s| s.to_string()),
            room_canonical_alias: room.canonical_alias().map(|c| c.to_string()),
            is_noisy,
            is_direct: room.is_direct().await?,
            is_encrypted: room.is_encrypted().await.ok(),
        };
        Ok(item)
    }
}
