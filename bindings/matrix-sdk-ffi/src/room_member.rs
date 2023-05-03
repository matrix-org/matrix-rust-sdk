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

    pub fn can_send_state(&self, state_event: StateEvent) -> bool {
        self.inner.can_send_state(state_event.into())
    }

    pub fn can_send_message(&self, event: MessageLikeEvent) -> bool {
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

impl From<StateEvent> for ruma::events::StateEventType {
    fn from(val: StateEvent) -> Self {
        use ruma::events::StateEventType;

        match val {
            StateEvent::PolicyRuleRoom => StateEventType::PolicyRuleRoom,
            StateEvent::PolicyRuleServer => StateEventType::PolicyRuleServer,
            StateEvent::PolicyRuleUser => StateEventType::PolicyRuleUser,
            StateEvent::RoomAliases => StateEventType::RoomAliases,
            StateEvent::RoomAvatar => StateEventType::RoomAvatar,
            StateEvent::RoomCanonicalAlias => StateEventType::RoomCanonicalAlias,
            StateEvent::RoomCreate => StateEventType::RoomCreate,
            StateEvent::RoomEncryption => StateEventType::RoomEncryption,
            StateEvent::RoomGuestAccess => StateEventType::RoomGuestAccess,
            StateEvent::RoomHistoryVisibility => StateEventType::RoomHistoryVisibility,
            StateEvent::RoomJoinRules => StateEventType::RoomJoinRules,
            StateEvent::RoomMemberEvent => StateEventType::RoomMember,
            StateEvent::RoomName => StateEventType::RoomName,
            StateEvent::RoomPinnedEvents => StateEventType::RoomPinnedEvents,
            StateEvent::RoomPowerLevels => StateEventType::RoomPowerLevels,
            StateEvent::RoomServerAcl => StateEventType::RoomServerAcl,
            StateEvent::RoomThirdPartyInvite => StateEventType::RoomThirdPartyInvite,
            StateEvent::RoomTombstone => StateEventType::RoomTombstone,
            StateEvent::RoomTopic => StateEventType::RoomTopic,
            StateEvent::SpaceChild => StateEventType::SpaceChild,
            StateEvent::SpaceParent => StateEventType::SpaceParent,
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum MessageLikeEvent {
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

impl From<MessageLikeEvent> for ruma::events::MessageLikeEventType {
    fn from(val: MessageLikeEvent) -> Self {
        use ruma::events::MessageLikeEventType;

        match val {
            MessageLikeEvent::CallAnswer => MessageLikeEventType::CallAnswer,
            MessageLikeEvent::CallInvite => MessageLikeEventType::CallInvite,
            MessageLikeEvent::CallHangup => MessageLikeEventType::CallHangup,
            MessageLikeEvent::CallCandidates => MessageLikeEventType::CallCandidates,
            MessageLikeEvent::KeyVerificationReady => MessageLikeEventType::KeyVerificationReady,
            MessageLikeEvent::KeyVerificationStart => MessageLikeEventType::KeyVerificationStart,
            MessageLikeEvent::KeyVerificationCancel => MessageLikeEventType::KeyVerificationCancel,
            MessageLikeEvent::KeyVerificationAccept => MessageLikeEventType::KeyVerificationAccept,
            MessageLikeEvent::KeyVerificationKey => MessageLikeEventType::KeyVerificationKey,
            MessageLikeEvent::KeyVerificationMac => MessageLikeEventType::KeyVerificationMac,
            MessageLikeEvent::KeyVerificationDone => MessageLikeEventType::KeyVerificationDone,
            MessageLikeEvent::ReactionSent => MessageLikeEventType::Reaction,
            MessageLikeEvent::RoomEncrypted => MessageLikeEventType::RoomEncrypted,
            MessageLikeEvent::RoomMessage => MessageLikeEventType::RoomMessage,
            MessageLikeEvent::RoomRedaction => MessageLikeEventType::RoomRedaction,
            MessageLikeEvent::Sticker => MessageLikeEventType::Sticker,
        }
    }
}
