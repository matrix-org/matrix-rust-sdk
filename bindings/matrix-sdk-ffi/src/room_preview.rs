use matrix_sdk::room_preview::RoomPreview as SdkRoomPreview;
use ruma::space::SpaceRoomJoinRule;

use crate::{client::JoinRule, error::ClientError, room::Membership};

/// A room preview for a room. It's intended to be used to represent rooms that
/// aren't joined yet.
#[derive(uniffi::Record)]
pub struct RoomPreview {
    pub info: RoomPreviewInfo,
}

impl RoomPreview {
    pub(crate) fn try_from_sdk(room_preview: SdkRoomPreview) -> Result<Self, ClientError> {
        let info = RoomPreviewInfo {
            room_id: room_preview.room_id.to_string(),
            canonical_alias: room_preview.canonical_alias.map(|alias| alias.to_string()),
            name: room_preview.name,
            topic: room_preview.topic,
            avatar_url: room_preview.avatar_url.map(|url| url.to_string()),
            num_joined_members: room_preview.num_joined_members,
            room_type: room_preview.room_type.map(|room_type| room_type.to_string()),
            is_history_world_readable: room_preview.is_world_readable,
            membership: room_preview
                .state
                .map(|state| state.into())
                .unwrap_or_else(|| Membership::Left),
            join_rule: room_preview.join_rule.into(),
        };

        Ok(Self { info })
    }
}

/// The preview of a room, be it invited/joined/left, or not.
#[derive(uniffi::Record)]
pub struct RoomPreviewInfo {
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
    pub membership: Membership,
    /// is the join rule public for this room?
    pub join_rule: JoinRule,
}

impl From<SpaceRoomJoinRule> for JoinRule {
    fn from(join_rule: SpaceRoomJoinRule) -> Self {
        match join_rule {
            SpaceRoomJoinRule::Public => JoinRule::Public,
            SpaceRoomJoinRule::Private => JoinRule::Private,
            SpaceRoomJoinRule::Invite => JoinRule::Invite,
            SpaceRoomJoinRule::Knock => JoinRule::Knock,
            SpaceRoomJoinRule::KnockRestricted => JoinRule::KnockRestricted { rules: Vec::new() },
            SpaceRoomJoinRule::Restricted => JoinRule::Restricted { rules: Vec::new() },
            // For the `_Custom` case, assume it's private
            _ => JoinRule::Private,
        }
    }
}
