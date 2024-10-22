use matrix_sdk::{
    room_preview::{
        JoinRoomPreviewAction as SdkJoinRoomPreviewAction, LeaveRoomPreviewAction as SdkLeaveRoomPreviewAction, RoomPreview as SdkRoomPreview,
        RoomPreviewActions,
    },
    Client, RoomState,
};
use ruma::space::SpaceRoomJoinRule;

use crate::{client::JoinRule, error::ClientError, room::Membership};

#[derive(uniffi::Record)]
pub struct RoomPreview {
    pub info: RoomPreviewInfo,
    pub room_preview_actions: RoomPreviewActions,
    pub(crate) sdk_info: matrix_sdk::room_preview::RoomPreview,
}

#[derive(uniffi::Enum)]
pub enum RoomPreviewAction {
    Invited { join: JoinRoomPreviewAction, leave: LeaveRoomPreviewAction },
    Knocked { leave: LeaveRoomPreviewAction },
}

#[derive(uniffi::Object)]
pub struct JoinRoomPreviewAction {
    inner: SdkJoinRoomPreviewAction,
}

#[matrix_sdk_ffi_macros::export]
impl JoinRoomPreviewAction {
    async fn run(&self) {
        self.inner.run().unwrap()
    }
}

impl From<matrix_sdk::room_preview::RoomPreviewActions> for RoomPreviewAction {
    fn from(actions: RoomPreviewActions) -> Self {
        match actions {
            RoomPreviewActions::Invited { join, leave } => Self::Invited { join, leave },
            RoomPreviewActions::Knocked { leave } => Self::Knocked { leave },
        }
    }
}

struct

impl RoomPreview {
    pub(crate) fn try_from_sdk(info: SdkRoomPreview, client: Client) -> Result<Self, ClientError> {
        let info = RoomPreviewInfo {
            room_id: info.room_id.to_string(),
            canonical_alias: info.canonical_alias.map(|alias| alias.to_string()),
            name: info.name,
            topic: info.topic,
            avatar_url: info.avatar_url.map(|url| url.to_string()),
            num_joined_members: info.num_joined_members,
            room_type: info.room_type.map(|room_type| room_type.to_string()),
            is_history_world_readable: info.is_world_readable,
            membership: info.state.map(|state| state.into()).unwrap_or_else(|| Membership::Left),
            join_rule: info.join_rule.into(),
        };

        let room_preview_actions = match info.membership {
            Membership::Invited => RoomPreviewActions::Invited {
                join: JoinRoomPreviewAction::new(client.clone()),
                leave: LeaveRoomPreviewAction::new(client.clone()),
            },
            Membership::Knocked => {
                RoomPreviewActions::Knocked { leave: LeaveRoomPreviewAction::new(client.clone()) }
            }
            _ => {
                return Err(ClientError::new(format!(
                    "The room preview had membership {:?} instead of Invited or Knocked.",
                    info.membership
                )))
            }
        };
        Ok(Self { info, room_preview_actions })
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
