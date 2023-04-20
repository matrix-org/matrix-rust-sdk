use std::sync::Arc;

use anyhow::{bail, Context};
use matrix_sdk::room::Room;
use ruma::{
    api::client::push::get_notifications::v3::Notification,
    events::{AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent},
};

// I need to make this an enum of structs just like TimelineEventContent
pub struct NotificationEventContent(pub(crate) AnyMessageLikeEventContent);

impl NotificationEventContent {}

impl TryFrom<AnySyncMessageLikeEvent> for NotificationEventContent {
    type Error = anyhow::Error;

    fn try_from(value: AnySyncMessageLikeEvent) -> anyhow::Result<Self> {
        let event = match value {
            AnySyncMessageLikeEvent::CallAnswer(content) => AnyMessageLikeEventContent::CallAnswer(
                content.as_original().context("Value not found")?.content.clone(),
            ),
            AnySyncMessageLikeEvent::CallInvite(content) => AnyMessageLikeEventContent::CallInvite(
                content.as_original().context("Value not found")?.content.clone(),
            ),
            AnySyncMessageLikeEvent::CallHangup(content) => AnyMessageLikeEventContent::CallHangup(
                content.as_original().context("Value not found")?.content.clone(),
            ),
            AnySyncMessageLikeEvent::CallCandidates(content) => {
                AnyMessageLikeEventContent::CallCandidates(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::KeyVerificationReady(content) => {
                AnyMessageLikeEventContent::KeyVerificationReady(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::KeyVerificationStart(content) => {
                AnyMessageLikeEventContent::KeyVerificationStart(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::KeyVerificationCancel(content) => {
                AnyMessageLikeEventContent::KeyVerificationCancel(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::KeyVerificationAccept(content) => {
                AnyMessageLikeEventContent::KeyVerificationAccept(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::KeyVerificationKey(content) => {
                AnyMessageLikeEventContent::KeyVerificationKey(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::KeyVerificationMac(content) => {
                AnyMessageLikeEventContent::KeyVerificationMac(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::KeyVerificationDone(content) => {
                AnyMessageLikeEventContent::KeyVerificationDone(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::Reaction(content) => AnyMessageLikeEventContent::Reaction(
                content.as_original().context("Value not found")?.content.clone(),
            ),
            AnySyncMessageLikeEvent::RoomEncrypted(content) => {
                AnyMessageLikeEventContent::RoomEncrypted(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::RoomMessage(content) => {
                AnyMessageLikeEventContent::RoomMessage(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::RoomRedaction(content) => {
                AnyMessageLikeEventContent::RoomRedaction(
                    content.as_original().context("Value not found")?.content.clone(),
                )
            }
            AnySyncMessageLikeEvent::Sticker(content) => AnyMessageLikeEventContent::Sticker(
                content.as_original().context("Value not found")?.content.clone(),
            ),
            _ => bail!("Unsupported event type"),
        };
        Ok(Self(event))
    }
}

#[allow(dead_code)]
pub struct NotificationService {
    base_path: String,
    user_id: String,
}
pub struct NotificationItem {
    pub event_content: Arc<NotificationEventContent>,
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
    pub(crate) async fn new(notification: &Notification, room: &Room) -> anyhow::Result<Self> {
        let deserialized_event = notification.event.deserialize()?;

        let sender = room.get_member(deserialized_event.sender()).await?;
        let mut sender_display_name = None;
        let mut sender_avatar_url = None;
        if let Some(sender) = sender {
            sender_display_name = sender.display_name().map(|s| s.to_owned());
            sender_avatar_url = sender.avatar_url().map(|s| s.to_string());
        }

        let is_noisy =
            notification.actions.iter().any(|a| a.sound().is_some() && a.should_notify());

        let event_content: NotificationEventContent = match deserialized_event {
            AnySyncTimelineEvent::MessageLike(event) => event,
            AnySyncTimelineEvent::State(_) => {
                bail!("State events can't be notifications")
            }
        }
        .try_into()?;

        let item = Self {
            event_content: Arc::new(event_content),
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
