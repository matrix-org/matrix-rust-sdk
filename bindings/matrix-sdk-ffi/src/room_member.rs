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

    pub fn can_redact(&self) -> bool {
        self.inner.can_redact()
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

#[derive(Clone, uniffi::Enum)]
pub enum StateEventType {
    CallMember,
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
    RoomMemberEvent,
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

impl From<StateEventType> for ruma::events::StateEventType {
    fn from(val: StateEventType) -> Self {
        match val {
            StateEventType::CallMember => Self::CallMember,
            StateEventType::PolicyRuleRoom => Self::PolicyRuleRoom,
            StateEventType::PolicyRuleServer => Self::PolicyRuleServer,
            StateEventType::PolicyRuleUser => Self::PolicyRuleUser,
            StateEventType::RoomAliases => Self::RoomAliases,
            StateEventType::RoomAvatar => Self::RoomAvatar,
            StateEventType::RoomCanonicalAlias => Self::RoomCanonicalAlias,
            StateEventType::RoomCreate => Self::RoomCreate,
            StateEventType::RoomEncryption => Self::RoomEncryption,
            StateEventType::RoomGuestAccess => Self::RoomGuestAccess,
            StateEventType::RoomHistoryVisibility => Self::RoomHistoryVisibility,
            StateEventType::RoomJoinRules => Self::RoomJoinRules,
            StateEventType::RoomMemberEvent => Self::RoomMember,
            StateEventType::RoomName => Self::RoomName,
            StateEventType::RoomPinnedEvents => Self::RoomPinnedEvents,
            StateEventType::RoomPowerLevels => Self::RoomPowerLevels,
            StateEventType::RoomServerAcl => Self::RoomServerAcl,
            StateEventType::RoomThirdPartyInvite => Self::RoomThirdPartyInvite,
            StateEventType::RoomTombstone => Self::RoomTombstone,
            StateEventType::RoomTopic => Self::RoomTopic,
            StateEventType::SpaceChild => Self::SpaceChild,
            StateEventType::SpaceParent => Self::SpaceParent,
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum MessageLikeEventType {
    CallAnswer,
    CallInvite,
    CallHangup,
    CallCandidates,
    KeyVerificationReady,
    KeyVerificationStart,
    KeyVerificationCancel,
    KeyVerificationAccept,
    KeyVerificationKey,
    KeyVerificationMac,
    KeyVerificationDone,
    ReactionSent,
    RoomEncrypted,
    RoomMessage,
    RoomRedaction,
    Sticker,
}

impl From<MessageLikeEventType> for ruma::events::MessageLikeEventType {
    fn from(val: MessageLikeEventType) -> Self {
        match val {
            MessageLikeEventType::CallAnswer => Self::CallAnswer,
            MessageLikeEventType::CallInvite => Self::CallInvite,
            MessageLikeEventType::CallHangup => Self::CallHangup,
            MessageLikeEventType::CallCandidates => Self::CallCandidates,
            MessageLikeEventType::KeyVerificationReady => Self::KeyVerificationReady,
            MessageLikeEventType::KeyVerificationStart => Self::KeyVerificationStart,
            MessageLikeEventType::KeyVerificationCancel => Self::KeyVerificationCancel,
            MessageLikeEventType::KeyVerificationAccept => Self::KeyVerificationAccept,
            MessageLikeEventType::KeyVerificationKey => Self::KeyVerificationKey,
            MessageLikeEventType::KeyVerificationMac => Self::KeyVerificationMac,
            MessageLikeEventType::KeyVerificationDone => Self::KeyVerificationDone,
            MessageLikeEventType::ReactionSent => Self::Reaction,
            MessageLikeEventType::RoomEncrypted => Self::RoomEncrypted,
            MessageLikeEventType::RoomMessage => Self::RoomMessage,
            MessageLikeEventType::RoomRedaction => Self::RoomRedaction,
            MessageLikeEventType::Sticker => Self::Sticker,
        }
    }
}
