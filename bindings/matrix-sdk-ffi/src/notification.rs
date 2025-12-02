use std::{collections::HashMap, sync::Arc};

use matrix_sdk_ui::notification_client::{
    NotificationClient as SdkNotificationClient, NotificationEvent as SdkNotificationEvent,
    NotificationItem as SdkNotificationItem, NotificationStatus as SdkNotificationStatus,
};
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};

use crate::{
    client::{Client, JoinRule},
    error::ClientError,
    event::TimelineEvent,
    room::Room,
};

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
    pub topic: Option<String>,
    pub join_rule: Option<JoinRule>,
    pub joined_members_count: u64,
    pub is_encrypted: Option<bool>,
    pub is_direct: bool,
    pub is_space: bool,
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
    pub thread_id: Option<String>,

    /// The push actions for this notification (notify, sound, highlight, etc.).
    pub actions: Option<Vec<crate::notification_settings::Action>>,
}

impl NotificationItem {
    fn from_inner(item: SdkNotificationItem) -> Self {
        let event = match item.event {
            SdkNotificationEvent::Timeline(event) => {
                NotificationEvent::Timeline { event: Arc::new(TimelineEvent(event)) }
            }
            SdkNotificationEvent::Invite(event) => {
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
                topic: item.room_topic,
                join_rule: item.room_join_rule.map(TryInto::try_into).transpose().ok().flatten(),
                joined_members_count: item.joined_members_count,
                is_encrypted: item.is_room_encrypted,
                is_direct: item.is_direct_message_room,
                is_space: item.is_space,
            },
            is_noisy: item.is_noisy,
            has_mention: item.has_mention,
            thread_id: item.thread_id.map(|t| t.to_string()),
            actions: item
                .actions
                .map(|a| a.into_iter().filter_map(|action| action.try_into().ok()).collect()),
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(uniffi::Enum)]
pub enum NotificationStatus {
    /// The event has been found and was not filtered out.
    Event { item: NotificationItem },
    /// The event couldn't be found in the network queries used to find it.
    EventNotFound,
    /// The event has been filtered out, either because of the user's push
    /// rules, or because the user which triggered it is ignored by the
    /// current user.
    EventFilteredOut,
}

impl From<SdkNotificationStatus> for NotificationStatus {
    fn from(item: SdkNotificationStatus) -> Self {
        match item {
            SdkNotificationStatus::Event(item) => {
                NotificationStatus::Event { item: NotificationItem::from_inner(*item) }
            }
            SdkNotificationStatus::EventNotFound => NotificationStatus::EventNotFound,
            SdkNotificationStatus::EventFilteredOut => NotificationStatus::EventFilteredOut,
        }
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(uniffi::Enum)]
pub enum BatchNotificationResult {
    /// We have more detailed information about the notification.
    Ok { status: NotificationStatus },
    /// An error occurred while trying to fetch the notification.
    Error {
        /// The error message observed while handling a specific notification.
        message: String,
    },
}

#[derive(uniffi::Object)]
pub struct NotificationClient {
    pub(crate) inner: SdkNotificationClient,

    /// A reference to the FFI client.
    ///
    /// Note: we do this to make it so that the FFI `NotificationClient` keeps
    /// the FFI `Client` and thus the SDK `Client` alive. Otherwise, we
    /// would need to repeat the hack done in the FFI `Client::drop` method.
    pub(crate) client: Arc<Client>,
}

#[matrix_sdk_ffi_macros::export]
impl NotificationClient {
    /// Fetches a room by its ID using the in-memory state store backed client.
    ///
    /// Useful to retrieve room information after running the limited
    /// notification client sliding sync loop.
    pub fn get_room(&self, room_id: String) -> Result<Option<Arc<Room>>, ClientError> {
        let room_id = RoomId::parse(room_id)?;
        let sdk_room = self.inner.get_room(&room_id);
        let room = sdk_room
            .map(|room| Arc::new(Room::new(room, self.client.utd_hook_manager.get().cloned())));
        Ok(room)
    }

    /// Fetches the content of a notification.
    ///
    /// This will first try to get the notification using a short-lived sliding
    /// sync, and if the sliding-sync can't find the event, then it'll use a
    /// `/context` query to find the event with associated member information.
    ///
    /// An error result means that we couldn't resolve the notification; in that
    /// case, a dummy notification may be displayed instead.
    pub async fn get_notification(
        &self,
        room_id: String,
        event_id: String,
    ) -> Result<NotificationStatus, ClientError> {
        let room_id = RoomId::parse(room_id)?;
        let event_id = EventId::parse(event_id)?;

        let item =
            self.inner.get_notification(&room_id, &event_id).await.map_err(ClientError::from)?;

        Ok(item.into())
    }

    /// Get several notification items in a single batch.
    ///
    /// Returns an error if the flow failed when preparing to fetch the
    /// notifications, and a [`HashMap`] containing either a
    /// [`BatchNotificationResult`], that indicates if the notification was
    /// successfully fetched (in which case, it's a [`NotificationStatus`]), or
    /// an error message if it couldn't be fetched.
    pub async fn get_notifications(
        &self,
        requests: Vec<NotificationItemsRequest>,
    ) -> Result<HashMap<String, BatchNotificationResult>, ClientError> {
        let requests =
            requests.into_iter().map(TryInto::try_into).collect::<Result<Vec<_>, _>>()?;

        let items = self.inner.get_notifications(&requests).await?;

        let mut batch_result = HashMap::new();
        for (key, value) in items.into_iter() {
            let result = match value {
                Ok(status) => BatchNotificationResult::Ok { status: status.into() },
                Err(error) => BatchNotificationResult::Error { message: error.to_string() },
            };
            batch_result.insert(key.to_string(), result);
        }

        Ok(batch_result)
    }
}

/// A request for notification items grouped by their room.
#[derive(uniffi::Record)]
pub struct NotificationItemsRequest {
    room_id: String,
    event_ids: Vec<String>,
}

impl NotificationItemsRequest {
    /// The parsed [`OwnedRoomId`] to use with the SDK crates.
    pub fn room_id(&self) -> Result<OwnedRoomId, ClientError> {
        RoomId::parse(&self.room_id).map_err(ClientError::from)
    }

    /// The parsed [`OwnedEventId`] list to use with the SDK crates.
    pub fn event_ids(&self) -> Result<Vec<OwnedEventId>, ClientError> {
        self.event_ids
            .iter()
            .map(|id| EventId::parse(id).map_err(ClientError::from))
            .collect::<Result<Vec<_>, _>>()
    }
}

impl TryFrom<NotificationItemsRequest>
    for matrix_sdk_ui::notification_client::NotificationItemsRequest
{
    type Error = ClientError;
    fn try_from(value: NotificationItemsRequest) -> Result<Self, Self::Error> {
        Ok(Self { room_id: value.room_id()?, event_ids: value.event_ids()? })
    }
}
