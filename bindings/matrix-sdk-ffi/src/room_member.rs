use matrix_sdk::room::RoomMember as SdkRoomMember;

use super::RUNTIME;
use crate::ClientError;

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
            _ => todo!(
                "Handle Custom case: https://github.com/matrix-org/matrix-rust-sdk/issues/1254"
            ),
        }
    }
}

pub struct RoomMember {
    inner: SdkRoomMember,
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

    pub fn can_send(&self, state_event: StateEvent) -> bool {
        use ruma::events::room::power_levels::PowerLevelAction;
        self.inner.can_do(PowerLevelAction::SendState(state_event.into()))
    }
}

impl RoomMember {
    pub fn new(room_member: SdkRoomMember) -> Self {
        RoomMember { inner: room_member }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum StateEvent {
    PolicyRuleRoom,
    PolicyRuleServer,
    PolicyRuleUser,
    RoomAliases,
    RoomAvatar,
    RoomCanonicalAlias,
    RoomCreate,
    RoomEncryption,
    RoomGuestAccess,
    RoomHistoryVisibility,
    RoomJoinRules,
    RoomMember,
    RoomName,
    RoomPinnedEvents,
    RoomPowerLevels,
    RoomServerAcl,
    RoomThirdPartyInvite,
    RoomTombstone,
    RoomTopic,
    SpaceChild,
    SpaceParent,
}

impl Into<ruma::events::StateEventType> for StateEvent {
    fn into(self) -> ruma::events::StateEventType {
        match self {
            Self::PolicyRuleRoom => ruma::events::StateEventType::PolicyRuleRoom,
            Self::PolicyRuleServer => ruma::events::StateEventType::PolicyRuleServer,
            Self::PolicyRuleUser => ruma::events::StateEventType::PolicyRuleUser,
            Self::RoomAliases => ruma::events::StateEventType::RoomAliases,
            Self::RoomAvatar => ruma::events::StateEventType::RoomAvatar,
            Self::RoomCanonicalAlias => ruma::events::StateEventType::RoomCanonicalAlias,
            Self::RoomCreate => ruma::events::StateEventType::RoomCreate,
            Self::RoomEncryption => ruma::events::StateEventType::RoomEncryption,
            Self::RoomGuestAccess => ruma::events::StateEventType::RoomGuestAccess,
            Self::RoomHistoryVisibility => ruma::events::StateEventType::RoomHistoryVisibility,
            Self::RoomJoinRules => ruma::events::StateEventType::RoomJoinRules,
            Self::RoomMember => ruma::events::StateEventType::RoomMember,
            Self::RoomName => ruma::events::StateEventType::RoomName,
            Self::RoomPinnedEvents => ruma::events::StateEventType::RoomPinnedEvents,
            Self::RoomPowerLevels => ruma::events::StateEventType::RoomPowerLevels,
            Self::RoomServerAcl => ruma::events::StateEventType::RoomServerAcl,
            Self::RoomThirdPartyInvite => ruma::events::StateEventType::RoomThirdPartyInvite,
            Self::RoomTombstone => ruma::events::StateEventType::RoomTombstone,
            Self::RoomTopic => ruma::events::StateEventType::RoomTopic,
            Self::SpaceChild => ruma::events::StateEventType::SpaceChild,
            Self::SpaceParent => ruma::events::StateEventType::SpaceParent,
        }
    }
}
