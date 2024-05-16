use std::sync::Arc;

use matrix_sdk_ui::notification_client::{
    NotificationClient as MatrixNotificationClient, NotificationItem as MatrixNotificationItem,
};
use ruma::{EventId, RoomId};

use crate::{client::Client, error::ClientError, event::TimelineEvent};

#[derive(uniffi::Enum)]
pub enum NotificationEvent {
    Timeline { event: Arc<TimelineEvent> },
    Invite { sender: String },
}

#[derive(uniffi::Record)]
pub struct NotificationSenderInfo {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub is_name_ambiguous: bool,
}

#[derive(uniffi::Record)]
pub struct NotificationRoomInfo {
    pub display_name: String,
    pub avatar_url: Option<String>,
    pub canonical_alias: Option<String>,
    pub joined_members_count: u64,
    pub is_encrypted: Option<bool>,
    pub is_direct: bool,
}

#[derive(uniffi::Record)]
pub struct NotificationItem {
    pub event: NotificationEvent,

    pub sender_info: NotificationSenderInfo,
    pub room_info: NotificationRoomInfo,

    /// Is the notification supposed to be at the "noisy" level?
    /// Can be `None` if we couldn't determine this, because we lacked
    /// information to create a push context.
    pub is_noisy: Option<bool>,
    pub has_mention: Option<bool>,
}

impl NotificationItem {
    fn from_inner(item: MatrixNotificationItem) -> Self {
        let event = match item.event {
            matrix_sdk_ui::notification_client::NotificationEvent::Timeline(event) => {
                NotificationEvent::Timeline { event: Arc::new(TimelineEvent(event)) }
            }
            matrix_sdk_ui::notification_client::NotificationEvent::Invite(event) => {
                NotificationEvent::Invite { sender: event.sender.to_string() }
            }
        };

        Self {
            event,
            sender_info: NotificationSenderInfo {
                display_name: item.sender_display_name,
                avatar_url: item.sender_avatar_url,
                is_name_ambiguous: item.is_sender_name_ambiguous,
            },
            room_info: NotificationRoomInfo {
                display_name: item.room_computed_display_name,
                avatar_url: item.room_avatar_url,
                canonical_alias: item.room_canonical_alias,
                joined_members_count: item.joined_members_count,
                is_encrypted: item.is_room_encrypted,
                is_direct: item.is_direct_message_room,
            },
            is_noisy: item.is_noisy,
            has_mention: item.has_mention,
        }
    }
}

#[derive(uniffi::Object)]
pub struct NotificationClient {
    pub(crate) inner: MatrixNotificationClient,

    /// A reference to the FFI client.
    ///
    /// Note: we do this to make it so that the FFI `NotificationClient` keeps
    /// the FFI `Client` and thus the SDK `Client` alive. Otherwise, we
    /// would need to repeat the hack done in the FFI `Client::drop` method.
    pub(crate) _client: Arc<Client>,
}

#[uniffi::export(async_runtime = "tokio")]
impl NotificationClient {
    /// See also documentation of
    /// `MatrixNotificationClient::get_notification`.
    pub async fn get_notification(
        &self,
        room_id: String,
        event_id: String,
    ) -> Result<Option<NotificationItem>, ClientError> {
        let room_id = RoomId::parse(room_id)?;
        let event_id = EventId::parse(event_id)?;

        let item =
            self.inner.get_notification(&room_id, &event_id).await.map_err(ClientError::from)?;

        if let Some(item) = item {
            Ok(Some(NotificationItem::from_inner(item)))
        } else {
            Ok(None)
        }
    }
}
