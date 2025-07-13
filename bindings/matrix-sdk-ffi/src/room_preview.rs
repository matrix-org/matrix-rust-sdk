use anyhow::Context as _;
use matrix_sdk::{room_preview::RoomPreview as SdkRoomPreview, Client};
use ruma::{room::RoomType as RumaRoomType, space::SpaceRoomJoinRule};
use tracing::warn;

use crate::{
    client::JoinRule,
    error::ClientError,
    room::{Membership, RoomHero},
    room_member::{RoomMember, RoomMemberWithSenderInfo},
    utils::AsyncRuntimeDropped,
};

/// A room preview for a room. It's intended to be used to represent rooms that
/// aren't joined yet.
#[derive(uniffi::Object)]
pub struct RoomPreview {
    inner: SdkRoomPreview,
    client: AsyncRuntimeDropped<Client>,
}

#[matrix_sdk_ffi_macros::export]
impl RoomPreview {
    /// Returns the room info the preview contains.
    pub fn info(&self) -> Result<RoomPreviewInfo, ClientError> {
        let info = &self.inner;
        Ok(RoomPreviewInfo {
            room_id: info.room_id.to_string(),
            canonical_alias: info.canonical_alias.as_ref().map(|alias| alias.to_string()),
            name: info.name.clone(),
            topic: info.topic.clone(),
            avatar_url: info.avatar_url.as_ref().map(|url| url.to_string()),
            num_joined_members: info.num_joined_members,
            num_active_members: info.num_active_members,
            room_type: info.room_type.as_ref().into(),
            is_history_world_readable: info.is_world_readable,
            membership: info.state.map(|state| state.into()),
            join_rule: info
                .join_rule
                .as_ref()
                .map(TryInto::try_into)
                .transpose()
                .map_err(|_| anyhow::anyhow!("unhandled SpaceRoomJoinRule kind"))?,
            is_direct: info.is_direct,
            heroes: info
                .heroes
                .as_ref()
                .map(|heroes| heroes.iter().map(|h| h.to_owned().into()).collect()),
        })
    }

    /// Leave the room if the room preview state is either joined, invited or
    /// knocked.
    ///
    /// If rejecting an invite then also forget it as an extra layer of
    /// protection against spam attacks.
    ///
    /// Will return an error otherwise.
    pub async fn leave(&self) -> Result<(), ClientError> {
        let room =
            self.client.get_room(&self.inner.room_id).context("missing room for a room preview")?;

        Ok(room.leave().await?)
    }

    /// Get the user who created the invite, if any.
    pub async fn inviter(&self) -> Option<RoomMember> {
        let room = self.client.get_room(&self.inner.room_id)?;
        let invite_details = room.invite_details().await.ok()?;
        invite_details.inviter.and_then(|m| m.try_into().ok())
    }

    /// Forget the room if we had access to it, and it was left or banned.
    pub async fn forget(&self) -> Result<(), ClientError> {
        let room =
            self.client.get_room(&self.inner.room_id).context("missing room for a room preview")?;
        room.forget().await?;
        Ok(())
    }

    /// Get the membership details for the current user.
    pub async fn own_membership_details(&self) -> Option<RoomMemberWithSenderInfo> {
        let room = self.client.get_room(&self.inner.room_id)?;
        room.member_with_sender_info(self.client.user_id()?).await.ok()?.try_into().ok()
    }
}

impl RoomPreview {
    pub(crate) fn new(client: AsyncRuntimeDropped<Client>, inner: SdkRoomPreview) -> Self {
        Self { client, inner }
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
    /// The number of active members, if known (joined + invited).
    pub num_active_members: Option<u64>,
    /// The room type (space, custom) or nothing, if it's a regular room.
    pub room_type: RoomType,
    /// Is the history world-readable for this room?
    pub is_history_world_readable: Option<bool>,
    /// The membership state for the current user, if known.
    pub membership: Option<Membership>,
    /// The join rule for this room (private, public, knock, etc.).
    pub join_rule: Option<JoinRule>,
    /// Whether the room is direct or not, if known.
    pub is_direct: Option<bool>,
    /// Room heroes.
    pub heroes: Option<Vec<RoomHero>>,
}

impl TryFrom<&SpaceRoomJoinRule> for JoinRule {
    type Error = ();

    fn try_from(join_rule: &SpaceRoomJoinRule) -> Result<Self, ()> {
        Ok(match join_rule {
            SpaceRoomJoinRule::Invite => JoinRule::Invite,
            SpaceRoomJoinRule::Knock => JoinRule::Knock,
            SpaceRoomJoinRule::Private => JoinRule::Private,
            SpaceRoomJoinRule::Restricted => JoinRule::Restricted { rules: Vec::new() },
            SpaceRoomJoinRule::KnockRestricted => JoinRule::KnockRestricted { rules: Vec::new() },
            SpaceRoomJoinRule::Public => JoinRule::Public,
            SpaceRoomJoinRule::_Custom(_) => JoinRule::Custom { repr: join_rule.to_string() },
            _ => {
                warn!("unhandled SpaceRoomJoinRule: {join_rule}");
                return Err(());
            }
        })
    }
}

/// The type of room for a [`RoomPreviewInfo`].
#[derive(Debug, Clone, uniffi::Enum)]
pub enum RoomType {
    /// It's a plain chat room.
    Room,
    /// It's a space that can group several rooms.
    Space,
    /// It's a custom implementation.
    Custom { value: String },
}

impl From<Option<&RumaRoomType>> for RoomType {
    fn from(value: Option<&RumaRoomType>) -> Self {
        match value {
            Some(RumaRoomType::Space) => RoomType::Space,
            Some(RumaRoomType::_Custom(_)) => RoomType::Custom {
                // SAFETY: this was checked in the match branch above
                value: value.unwrap().to_string(),
            },
            _ => RoomType::Room,
        }
    }
}
