use matrix_sdk::room::{RoomMember as SdkRoomMember, RoomMemberRole};

use super::RUNTIME;
use crate::{
    event::{MessageLikeEventType, StateEventType},
    ClientError,
};

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

#[derive(uniffi::Object)]
pub struct RoomMember {
    pub(crate) inner: SdkRoomMember,
}

#[uniffi::export]
impl RoomMember {
    pub fn user_id(&self) -> String {
        self.inner.user_id().to_string()
    }

    pub fn display_name(&self) -> Option<String> {
        self.inner.display_name().map(|d| d.to_owned())
    }

    pub fn avatar_url(&self) -> Option<String> {
        self.inner.avatar_url().map(ToString::to_string)
    }

    pub fn membership(&self) -> MembershipState {
        self.inner.membership().to_owned().into()
    }

    pub fn is_name_ambiguous(&self) -> bool {
        self.inner.name_ambiguous()
    }

    pub fn power_level(&self) -> i64 {
        self.inner.power_level()
    }

    pub fn suggested_role_for_power_level(&self) -> RoomMemberRole {
        self.inner.suggested_role_for_power_level()
    }

    pub fn normalized_power_level(&self) -> i64 {
        self.inner.normalized_power_level()
    }

    pub fn is_ignored(&self) -> bool {
        self.inner.is_ignored()
    }

    pub fn is_account_user(&self) -> bool {
        self.inner.is_account_user()
    }

    /// Adds the room member to the current account data's ignore list
    /// which will ignore the user across all rooms.
    pub fn ignore(&self) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            self.inner.ignore().await?;
            Ok(())
        })
    }

    /// Removes the room member from the current account data's ignore list
    /// which will unignore the user across all rooms.
    pub fn unignore(&self) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            self.inner.unignore().await?;
            Ok(())
        })
    }

    pub fn can_ban(&self) -> bool {
        self.inner.can_ban()
    }

    pub fn can_invite(&self) -> bool {
        self.inner.can_invite()
    }

    pub fn can_kick(&self) -> bool {
        self.inner.can_kick()
    }

    pub fn can_redact_own(&self) -> bool {
        self.inner.can_redact_own()
    }

    pub fn can_redact_other(&self) -> bool {
        self.inner.can_redact_other()
    }

    pub fn can_send_state(&self, state_event: StateEventType) -> bool {
        self.inner.can_send_state(state_event.into())
    }

    pub fn can_send_message(&self, event: MessageLikeEventType) -> bool {
        self.inner.can_send_message(event.into())
    }

    pub fn can_trigger_room_notification(&self) -> bool {
        self.inner.can_trigger_room_notification()
    }
}

impl RoomMember {
    pub fn new(room_member: SdkRoomMember) -> Self {
        RoomMember { inner: room_member }
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
