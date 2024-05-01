use std::{collections::HashMap, sync::Arc};

use matrix_sdk::RoomState;
use ruma::OwnedMxcUri;

use crate::{
    notification_settings::RoomNotificationMode, room::Membership, room_member::RoomMember,
    timeline::EventTimelineItem,
};

#[derive(uniffi::Record)]
pub struct RoomInfo {
    id: String,
    /// The room's name from the room state event if received from sync, or one
    /// that's been computed otherwise.
    name: Option<String>,
    topic: Option<String>,
    avatar_url: Option<String>,
    is_direct: bool,
    is_public: bool,
    is_space: bool,
    is_tombstoned: bool,
    is_favourite: bool,
    canonical_alias: Option<String>,
    alternative_aliases: Vec<String>,
    membership: Membership,
    latest_event: Option<Arc<EventTimelineItem>>,
    /// Member who invited the current user to a room that's in the invited
    /// state.
    ///
    /// Can be missing if the room membership invite event is missing from the
    /// store.
    inviter: Option<RoomMember>,
    active_members_count: u64,
    invited_members_count: u64,
    joined_members_count: u64,
    user_power_levels: HashMap<String, i64>,
    highlight_count: u64,
    notification_count: u64,
    user_defined_notification_mode: Option<RoomNotificationMode>,
    has_room_call: bool,
    active_room_call_participants: Vec<String>,
    /// Whether this room has been explicitly marked as unread
    is_marked_unread: bool,
    /// "Interesting" messages received in that room, independently of the
    /// notification settings.
    num_unread_messages: u64,
    /// Events that will notify the user, according to their
    /// notification settings.
    num_unread_notifications: u64,
    /// Events causing mentions/highlights for the user, according to their
    /// notification settings.
    num_unread_mentions: u64,
}

impl RoomInfo {
    pub(crate) async fn new(
        room: &matrix_sdk::Room,
        avatar_url: Option<OwnedMxcUri>,
        latest_event: Option<Arc<EventTimelineItem>>,
    ) -> matrix_sdk::Result<Self> {
        let unread_notification_counts = room.unread_notification_counts();

        let power_levels_map = room.users_with_power_levels().await;
        let mut user_power_levels = HashMap::<String, i64>::new();
        for (id, level) in power_levels_map.iter() {
            user_power_levels.insert(id.to_string(), *level);
        }

        Ok(Self {
            id: room.room_id().to_string(),
            name: room.computed_display_name().await.ok().map(|name| name.to_string()),
            topic: room.topic(),
            avatar_url: avatar_url.map(Into::into),
            is_direct: room.is_direct().await?,
            is_public: room.is_public(),
            is_space: room.is_space(),
            is_tombstoned: room.is_tombstoned(),
            is_favourite: room.is_favourite(),
            canonical_alias: room.canonical_alias().map(Into::into),
            alternative_aliases: room.alt_aliases().into_iter().map(Into::into).collect(),
            membership: room.state().into(),
            latest_event,
            inviter: match room.state() {
                RoomState::Invited => room
                    .invite_details()
                    .await
                    .ok()
                    .and_then(|details| details.inviter)
                    .map(Into::into),
                _ => None,
            },
            active_members_count: room.active_members_count(),
            invited_members_count: room.invited_members_count(),
            joined_members_count: room.joined_members_count(),
            user_power_levels,
            highlight_count: unread_notification_counts.highlight_count,
            notification_count: unread_notification_counts.notification_count,
            user_defined_notification_mode: room
                .user_defined_notification_mode()
                .await
                .map(Into::into),
            has_room_call: room.has_active_room_call(),
            active_room_call_participants: room
                .active_room_call_participants()
                .iter()
                .map(|u| u.to_string())
                .collect(),
            is_marked_unread: room.is_marked_unread(),
            num_unread_messages: room.num_unread_messages(),
            num_unread_notifications: room.num_unread_notifications(),
            num_unread_mentions: room.num_unread_mentions(),
        })
    }
}
