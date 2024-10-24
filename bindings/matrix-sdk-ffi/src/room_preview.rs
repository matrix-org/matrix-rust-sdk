use std::sync::Arc;

use matrix_sdk::room_preview::RoomPreview as SdkRoomPreview;
use ruma::space::SpaceRoomJoinRule;

use crate::{client::JoinRule, error::ClientError, room::Membership};

/// A room preview for a room. It's intended to be used to represent rooms that
/// aren't joined yet.
#[derive(uniffi::Object)]
pub struct RoomPreview {
    inner: Arc<SdkRoomPreview>,
}

#[matrix_sdk_ffi_macros::export]
impl RoomPreview {
    /// Returns the room info the preview contains.
    pub fn info(&self) -> RoomPreviewInfo {
        let info = &self.inner;
        RoomPreviewInfo {
            room_id: info.room_id.to_string(),
            canonical_alias: info.canonical_alias.as_ref().map(|alias| alias.to_string()),
            name: info.name.clone(),
            topic: info.topic.clone(),
            avatar_url: info.avatar_url.as_ref().map(|url| url.to_string()),
            num_joined_members: info.num_joined_members,
            room_type: info.room_type.as_ref().map(|room_type| room_type.to_string()),
            is_history_world_readable: info.is_world_readable,
            membership: info.state.map(|state| state.into()),
            join_rule: info.join_rule.clone().into(),
        }
    }

    /// Leave the room if the room preview state is either joined, invited or
    /// knocked.
    ///
    /// Will return an error otherwise.
    pub async fn leave(&self) -> Result<(), ClientError> {
        self.inner.leave().await.map_err(Into::into)
    }
}

impl RoomPreview {
    pub(crate) fn from_sdk(room_preview: SdkRoomPreview) -> Self {
        Self { inner: Arc::new(room_preview) }
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
    /// The membership state for the current user, if known.
    pub membership: Option<Membership>,
    /// The join rule for this room (private, public, knock, etc.).
    pub join_rule: JoinRule,
}

impl From<SpaceRoomJoinRule> for JoinRule {
    fn from(join_rule: SpaceRoomJoinRule) -> Self {
        match join_rule {
            SpaceRoomJoinRule::Invite => JoinRule::Invite,
            SpaceRoomJoinRule::Knock => JoinRule::Knock,
            SpaceRoomJoinRule::Private => JoinRule::Private,
            SpaceRoomJoinRule::Restricted => JoinRule::Restricted { rules: Vec::new() },
            SpaceRoomJoinRule::KnockRestricted => JoinRule::KnockRestricted { rules: Vec::new() },
            SpaceRoomJoinRule::Public => JoinRule::Public,
            _ => {
                // Since we have no way to get the _Custom contents, assume it's private.
                // Warning! If new join rules are introduced we may be mapping wrong values
                // here, but there's no way to match
                // `SpaceRoomJoinRule::_Custom(_)` and have an exhaustive match.
                JoinRule::Private
            }
        }
    }
}
