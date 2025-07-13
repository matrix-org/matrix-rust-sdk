use matrix_sdk::room::{RoomMember as SdkRoomMember, RoomMemberRole};
use ruma::UserId;

use crate::error::{ClientError, NotYetImplemented};

#[derive(Clone, uniffi::Enum)]
pub enum MembershipState {
    /// The user is banned.
    Ban,

    /// The user has been invited.
    Invite,

    /// The user has joined.
    Join,

    /// The user has requested to join.
    Knock,

    /// The user has left.
    Leave,

    /// A custom membership state value.
    Custom { value: String },
}

impl TryFrom<matrix_sdk::ruma::events::room::member::MembershipState> for MembershipState {
    type Error = NotYetImplemented;

    fn try_from(
        m: matrix_sdk::ruma::events::room::member::MembershipState,
    ) -> Result<Self, Self::Error> {
        match m {
            matrix_sdk::ruma::events::room::member::MembershipState::Ban => {
                Ok(MembershipState::Ban)
            }
            matrix_sdk::ruma::events::room::member::MembershipState::Invite => {
                Ok(MembershipState::Invite)
            }
            matrix_sdk::ruma::events::room::member::MembershipState::Join => {
                Ok(MembershipState::Join)
            }
            matrix_sdk::ruma::events::room::member::MembershipState::Knock => {
                Ok(MembershipState::Knock)
            }
            matrix_sdk::ruma::events::room::member::MembershipState::Leave => {
                Ok(MembershipState::Leave)
            }
            matrix_sdk::ruma::events::room::member::MembershipState::_Custom(_) => {
                Ok(MembershipState::Custom { value: m.to_string() })
            }
            _ => {
                tracing::warn!("Other membership state change not yet implemented");
                Err(NotYetImplemented)
            }
        }
    }
}

#[matrix_sdk_ffi_macros::export]
pub fn suggested_role_for_power_level(power_level: i64) -> RoomMemberRole {
    // It's not possible to expose the constructor on the Enum through Uniffi ☹️
    RoomMemberRole::suggested_role_for_power_level(power_level)
}

#[matrix_sdk_ffi_macros::export]
pub fn suggested_power_level_for_role(role: RoomMemberRole) -> i64 {
    // It's not possible to expose methods on an Enum through Uniffi ☹️
    role.suggested_power_level()
}

/// Generates a `matrix.to` permalink to the given userID.
#[matrix_sdk_ffi_macros::export]
pub fn matrix_to_user_permalink(user_id: String) -> Result<String, ClientError> {
    let user_id = UserId::parse(user_id)?;
    Ok(user_id.matrix_to_uri().to_string())
}

#[derive(Clone, uniffi::Record)]
pub struct RoomMember {
    pub user_id: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
    pub membership: MembershipState,
    pub is_name_ambiguous: bool,
    pub power_level: i64,
    pub normalized_power_level: i64,
    pub is_ignored: bool,
    pub suggested_role_for_power_level: RoomMemberRole,
    pub membership_change_reason: Option<String>,
}

impl TryFrom<SdkRoomMember> for RoomMember {
    type Error = NotYetImplemented;

    fn try_from(m: SdkRoomMember) -> Result<Self, Self::Error> {
        Ok(RoomMember {
            user_id: m.user_id().to_string(),
            display_name: m.display_name().map(|s| s.to_owned()),
            avatar_url: m.avatar_url().map(|a| a.to_string()),
            membership: m.membership().clone().try_into()?,
            is_name_ambiguous: m.name_ambiguous(),
            power_level: m.power_level(),
            normalized_power_level: m.normalized_power_level(),
            is_ignored: m.is_ignored(),
            suggested_role_for_power_level: m.suggested_role_for_power_level(),
            membership_change_reason: m.event().reason().map(|s| s.to_owned()),
        })
    }
}

/// Contains the current user's room member info and the optional room member
/// info of the sender of the `m.room.member` event that this info represents.
#[derive(Clone, uniffi::Record)]
pub struct RoomMemberWithSenderInfo {
    /// The room member.
    room_member: RoomMember,
    /// The info of the sender of the event `room_member` is based on, if
    /// available.
    sender_info: Option<RoomMember>,
}

impl TryFrom<matrix_sdk::room::RoomMemberWithSenderInfo> for RoomMemberWithSenderInfo {
    type Error = ClientError;

    fn try_from(value: matrix_sdk::room::RoomMemberWithSenderInfo) -> Result<Self, Self::Error> {
        Ok(Self {
            room_member: value.room_member.try_into()?,
            sender_info: value.sender_info.map(|member| member.try_into()).transpose()?,
        })
    }
}
