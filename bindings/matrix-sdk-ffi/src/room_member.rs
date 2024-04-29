use matrix_sdk::room::{RoomMember as SdkRoomMember, RoomMemberRole};
use ruma::UserId;

use crate::error::ClientError;

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
}

impl From<matrix_sdk::ruma::events::room::member::MembershipState> for MembershipState {
    fn from(m: matrix_sdk::ruma::events::room::member::MembershipState) -> Self {
        match m {
            matrix_sdk::ruma::events::room::member::MembershipState::Ban => MembershipState::Ban,
            matrix_sdk::ruma::events::room::member::MembershipState::Invite => {
                MembershipState::Invite
            }
            matrix_sdk::ruma::events::room::member::MembershipState::Join => MembershipState::Join,
            matrix_sdk::ruma::events::room::member::MembershipState::Knock => {
                MembershipState::Knock
            }
            matrix_sdk::ruma::events::room::member::MembershipState::Leave => {
                MembershipState::Leave
            }
            _ => unimplemented!(
                "Handle Custom case: https://github.com/matrix-org/matrix-rust-sdk/issues/1254"
            ),
        }
    }
}

#[uniffi::export]
pub fn suggested_role_for_power_level(power_level: i64) -> RoomMemberRole {
    // It's not possible to expose the constructor on the Enum through Uniffi ☹️
    RoomMemberRole::suggested_role_for_power_level(power_level)
}

#[uniffi::export]
pub fn suggested_power_level_for_role(role: RoomMemberRole) -> i64 {
    // It's not possible to expose methods on an Enum through Uniffi ☹️
    role.suggested_power_level()
}

/// Generates a `matrix.to` permalink to the given userID.
#[uniffi::export]
pub fn matrix_to_user_permalink(user_id: String) -> Result<String, ClientError> {
    let user_id = UserId::parse(user_id)?;
    Ok(user_id.matrix_to_uri().to_string())
}

#[derive(uniffi::Record)]
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
}

impl From<SdkRoomMember> for RoomMember {
    fn from(m: SdkRoomMember) -> Self {
        RoomMember {
            user_id: m.user_id().to_string(),
            display_name: m.display_name().map(|s| s.to_owned()),
            avatar_url: m.avatar_url().map(|a| a.to_string()),
            membership: m.membership().clone().into(),
            is_name_ambiguous: m.name_ambiguous(),
            power_level: m.power_level(),
            normalized_power_level: m.normalized_power_level(),
            is_ignored: m.is_ignored(),
            suggested_role_for_power_level: m.suggested_role_for_power_level(),
        }
    }
}
