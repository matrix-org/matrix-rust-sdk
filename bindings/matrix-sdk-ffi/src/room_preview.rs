use matrix_sdk::{room_preview::RoomPreview as SdkRoomPreview, RoomState};
use ruma::{
    events::room::{history_visibility::HistoryVisibility, join_rules::JoinRule},
    OwnedRoomId,
};

/// The preview of a room, be it invited/joined/left, or not.
#[derive(uniffi::Record)]
pub struct RoomPreview {
    /// The room id for this room.
    pub room_id: String,
    /// The canonical alias for the room.
    pub canonical_alias: Option<String>,
    /// The room's name, if set.
    pub name: Option<String>,
    /// The room's topic, if set.
    pub topic: Option<String>,
    /// The MXC URI to the room's avatar, if set.
    pub avatar_url: Option<String>,
    /// The number of joined members.
    pub num_joined_members: u64,
    /// The room type (space, custom) or nothing, if it's a regular room.
    pub room_type: Option<String>,
    /// Is the history world-readable for this room?
    pub is_history_world_readable: bool,
    /// Is the room joined by the current user?
    pub is_joined: bool,
    /// Is the current user invited to this room?
    pub is_invited: bool,
    /// is the join rule public for this room?
    pub is_public: bool,
    /// Can we knock (or restricted-knock) to this room?
    pub can_knock: bool,
}

impl RoomPreview {
    pub(crate) fn from_sdk(room_id: OwnedRoomId, preview: SdkRoomPreview) -> Self {
        Self {
            room_id: room_id.to_string(),
            canonical_alias: preview.canonical_alias.map(|alias| alias.to_string()),
            name: preview.name,
            topic: preview.topic,
            avatar_url: preview.avatar_url.map(|url| url.to_string()),
            num_joined_members: preview.num_joined_members,
            room_type: preview.room_type.map(|room_type| room_type.to_string()),
            is_history_world_readable: preview.history_visibility
                == HistoryVisibility::WorldReadable,
            is_joined: preview.state.map_or(false, |state| state == RoomState::Joined),
            is_invited: preview.state.map_or(false, |state| state == RoomState::Invited),
            is_public: preview.join_rule == JoinRule::Public,
            can_knock: matches!(preview.join_rule, JoinRule::KnockRestricted(_) | JoinRule::Knock),
        }
    }
}
