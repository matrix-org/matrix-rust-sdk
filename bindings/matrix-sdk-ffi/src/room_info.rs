use std::sync::Arc;

use matrix_sdk::RoomState;
use matrix_sdk_ui::room_list_service::Room;

use crate::{room::Membership, room_member::RoomMember, timeline::EventTimelineItem};

#[derive(uniffi::Record)]
pub struct RoomInfo {
    id: String,
    name: Option<String>,
    topic: Option<String>,
    avatar_url: Option<String>,
    is_direct: bool,
    /// Whether the room is encrypted.
    ///
    /// Currently always `None` (unknown) for invited rooms.
    is_encrypted: Option<bool>,
    is_public: bool,
    is_space: bool,
    is_tombstoned: bool,
    canonical_alias: Option<String>,
    alternative_aliases: Vec<String>,
    membership: Membership,
    latest_event: Option<Arc<EventTimelineItem>>,
    inviter: Option<Arc<RoomMember>>,
    active_members_count: u64,
    invited_members_count: u64,
    joined_members_count: u64,
    highlight_count: u64,
    notification_count: u64,
}

impl RoomInfo {
    pub(crate) async fn new(room: &Room) -> matrix_sdk::Result<Self> {
        let inner = room.inner_room();
        let unread_notification_counts = inner.unread_notification_counts();

        Ok(Self {
            id: inner.room_id().to_string(),
            name: inner.name(),
            topic: inner.topic(),
            avatar_url: inner.avatar_url().map(Into::into),
            is_direct: inner.is_direct().await?,
            is_encrypted: match inner.state() {
                RoomState::Invited => None,
                _ => Some(inner.is_encrypted().await?),
            },
            is_public: inner.is_public(),
            is_space: inner.is_space(),
            is_tombstoned: inner.is_tombstoned(),
            canonical_alias: inner.canonical_alias().map(Into::into),
            alternative_aliases: inner.alt_aliases().into_iter().map(Into::into).collect(),
            membership: inner.state().into(),
            latest_event: room.latest_event().await.map(EventTimelineItem).map(Arc::new),
            inviter: match inner.state() {
                RoomState::Invited => inner
                    .invite_details()
                    .await?
                    .inviter
                    .map(|inner| Arc::new(RoomMember { inner })),
                _ => None,
            },
            active_members_count: inner.active_members_count(),
            invited_members_count: inner.invited_members_count(),
            joined_members_count: inner.joined_members_count(),
            highlight_count: unread_notification_counts.highlight_count,
            notification_count: unread_notification_counts.notification_count,
        })
    }
}
