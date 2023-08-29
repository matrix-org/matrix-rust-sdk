use std::sync::Arc;

use matrix_sdk::RoomState;

use crate::{
    notification_settings::RoomNotificationMode, room::Membership, room_member::RoomMember,
    timeline::EventTimelineItem,
};

#[derive(uniffi::Record)]
pub struct RoomInfo {
    id: String,
    name: Option<String>,
    topic: Option<String>,
    avatar_url: Option<String>,
    is_direct: bool,
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
    notification_mode: Option<RoomNotificationMode>,
}

impl RoomInfo {
    pub(crate) async fn new(
        room: &matrix_sdk::Room,
        latest_event: Option<Arc<EventTimelineItem>>,
    ) -> matrix_sdk::Result<Self> {
        let unread_notification_counts = room.unread_notification_counts();

        Ok(Self {
            id: room.room_id().to_string(),
            name: room.name(),
            topic: room.topic(),
            avatar_url: room.avatar_url().map(Into::into),
            is_direct: room.is_direct().await?,
            is_public: room.is_public(),
            is_space: room.is_space(),
            is_tombstoned: room.is_tombstoned(),
            canonical_alias: room.canonical_alias().map(Into::into),
            alternative_aliases: room.alt_aliases().into_iter().map(Into::into).collect(),
            membership: room.state().into(),
            latest_event,
            inviter: match room.state() {
                RoomState::Invited => {
                    room.invite_details().await?.inviter.map(|inner| Arc::new(RoomMember { inner }))
                }
                _ => None,
            },
            active_members_count: room.active_members_count(),
            invited_members_count: room.invited_members_count(),
            joined_members_count: room.joined_members_count(),
            highlight_count: unread_notification_counts.highlight_count,
            notification_count: unread_notification_counts.notification_count,
            notification_mode: room.notification_mode().await.map(Into::into),
        })
    }
}
