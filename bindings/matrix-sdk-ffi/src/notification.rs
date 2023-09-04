use std::sync::Arc;

use matrix_sdk_ui::notification_client::{
    NotificationClient as MatrixNotificationClient,
    NotificationClientBuilder as MatrixNotificationClientBuilder,
    NotificationItem as MatrixNotificationItem, NotificationProcessSetup,
};
use ruma::{EventId, RoomId};

use crate::{error::ClientError, event::TimelineEvent, helpers::unwrap_or_clone_arc, RUNTIME};

#[derive(uniffi::Enum)]
pub enum NotificationEvent {
    Timeline { event: Arc<TimelineEvent> },
    Invite { sender: String },
}

#[derive(uniffi::Record)]
pub struct NotificationSenderInfo {
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
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
            },
            room_info: NotificationRoomInfo {
                display_name: item.room_display_name,
                avatar_url: item.room_avatar_url,
                canonical_alias: item.room_canonical_alias,
                joined_members_count: item.joined_members_count,
                is_encrypted: item.is_room_encrypted,
                is_direct: item.is_direct_message_room,
            },
            is_noisy: item.is_noisy,
        }
    }
}

#[derive(Clone, uniffi::Object)]
pub struct NotificationClientBuilder {
    builder: MatrixNotificationClientBuilder,
}

impl NotificationClientBuilder {
    pub(crate) fn new(
        client: matrix_sdk::Client,
        process_setup: NotificationProcessSetup,
    ) -> Result<Arc<Self>, ClientError> {
        let builder = RUNTIME
            .block_on(async { MatrixNotificationClient::builder(client, process_setup).await })?;
        Ok(Arc::new(Self { builder }))
    }
}

#[uniffi::export]
impl NotificationClientBuilder {
    /// Filter out the notification event according to the push rules present in
    /// the event.
    pub fn filter_by_push_rules(self: Arc<Self>) -> Arc<Self> {
        let this = unwrap_or_clone_arc(self);
        let builder = this.builder.filter_by_push_rules();
        Arc::new(Self { builder })
    }

    /// Automatically retry decryption once, if the notification was received
    /// encrypted.
    pub fn retry_decryption(self: Arc<Self>) -> Arc<Self> {
        let this = unwrap_or_clone_arc(self);
        let builder = this.builder.retry_decryption();
        Arc::new(Self { builder })
    }

    pub fn finish(self: Arc<Self>) -> Arc<NotificationClient> {
        let this = unwrap_or_clone_arc(self);
        Arc::new(NotificationClient { inner: this.builder.build() })
    }
}

#[derive(uniffi::Object)]
pub struct NotificationClient {
    inner: MatrixNotificationClient,
}

#[uniffi::export]
impl NotificationClient {
    /// See also documentation of
    /// `MatrixNotificationClient::get_notification`.
    pub fn get_notification(
        &self,
        room_id: String,
        event_id: String,
    ) -> Result<Option<NotificationItem>, ClientError> {
        let room_id = RoomId::parse(room_id)?;
        let event_id = EventId::parse(event_id)?;
        RUNTIME.block_on(async move {
            let item = self
                .inner
                .get_notification(&room_id, &event_id)
                .await
                .map_err(ClientError::from)?;
            if let Some(item) = item {
                Ok(Some(NotificationItem::from_inner(item)))
            } else {
                Ok(None)
            }
        })
    }
}
