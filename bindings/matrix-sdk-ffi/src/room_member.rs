use matrix_sdk::room::RoomMember as SdkRoomMember;
use ruma::events::room::power_levels::PowerLevelAction;

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

    pub fn can_ban_users(&self) -> bool {
        self.inner.can_do(PowerLevelAction::Ban)
    }

    pub fn can_invite_users(&self) -> bool {
        self.inner.can_do(PowerLevelAction::Invite)
    }

    pub fn can_kick_users(&self) -> bool {
        self.inner.can_do(PowerLevelAction::Kick)
    }

    pub fn can_redact_events(&self) -> bool {
        self.inner.can_do(PowerLevelAction::Redact)
    }

    pub fn can_send_state_event(&self, state_event: StateEvent) -> bool {
        self.inner.can_do(PowerLevelAction::SendState(state_event.into()))
    }

    pub fn can_send_event(&self, event: MessageLikeEvent) -> bool {
        self.inner.can_do(PowerLevelAction::SendMessage(event.into()))
    }

    pub fn can_trigger_notification(&self, notification: NotificationPowerLevel) -> bool {
        self.inner.can_do(PowerLevelAction::TriggerNotification(notification.into()))
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

impl Into<ruma::events::StateEventType> for StateEvent {
    fn into(self) -> ruma::events::StateEventType {
        use ruma::events::StateEventType;

        match self {
            Self::PolicyRuleRoom => StateEventType::PolicyRuleRoom,
            Self::PolicyRuleServer => StateEventType::PolicyRuleServer,
            Self::PolicyRuleUser => StateEventType::PolicyRuleUser,
            Self::RoomAliases => StateEventType::RoomAliases,
            Self::RoomAvatar => StateEventType::RoomAvatar,
            Self::RoomCanonicalAlias => StateEventType::RoomCanonicalAlias,
            Self::RoomCreate => StateEventType::RoomCreate,
            Self::RoomEncryption => StateEventType::RoomEncryption,
            Self::RoomGuestAccess => StateEventType::RoomGuestAccess,
            Self::RoomHistoryVisibility => StateEventType::RoomHistoryVisibility,
            Self::RoomJoinRules => StateEventType::RoomJoinRules,
            Self::RoomMemberEvent => StateEventType::RoomMember,
            Self::RoomName => StateEventType::RoomName,
            Self::RoomPinnedEvents => StateEventType::RoomPinnedEvents,
            Self::RoomPowerLevels => StateEventType::RoomPowerLevels,
            Self::RoomServerAcl => StateEventType::RoomServerAcl,
            Self::RoomThirdPartyInvite => StateEventType::RoomThirdPartyInvite,
            Self::RoomTombstone => StateEventType::RoomTombstone,
            Self::RoomTopic => StateEventType::RoomTopic,
            Self::SpaceChild => StateEventType::SpaceChild,
            Self::SpaceParent => StateEventType::SpaceParent,
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

impl Into<ruma::events::MessageLikeEventType> for MessageLikeEvent {
    fn into(self) -> ruma::events::MessageLikeEventType {
        use ruma::events::MessageLikeEventType;

        match self {
            Self::CallAnswer => MessageLikeEventType::CallAnswer,
            Self::CallInvite => MessageLikeEventType::CallInvite,
            Self::CallHangup => MessageLikeEventType::CallHangup,
            Self::CallCandidates => MessageLikeEventType::CallCandidates,
            Self::KeyVerificationReady => MessageLikeEventType::KeyVerificationReady,
            Self::KeyVerificationStart => MessageLikeEventType::KeyVerificationStart,
            Self::KeyVerificationCancel => MessageLikeEventType::KeyVerificationCancel,
            Self::KeyVerificationAccept => MessageLikeEventType::KeyVerificationAccept,
            Self::KeyVerificationKey => MessageLikeEventType::KeyVerificationKey,
            Self::KeyVerificationMac => MessageLikeEventType::KeyVerificationMac,
            Self::KeyVerificationDone => MessageLikeEventType::KeyVerificationDone,
            Self::ReactionSent => MessageLikeEventType::Reaction,
            Self::RoomEncrypted => MessageLikeEventType::RoomEncrypted,
            Self::RoomMessage => MessageLikeEventType::RoomMessage,
            Self::RoomRedaction => MessageLikeEventType::RoomRedaction,
            Self::Sticker => MessageLikeEventType::Sticker,
        }
    }
}
#[derive(Clone, uniffi::Enum)]
pub enum NotificationPowerLevel {
    Room,
}

impl Into<ruma::events::room::power_levels::NotificationPowerLevelType> for NotificationPowerLevel {
    fn into(self) -> ruma::events::room::power_levels::NotificationPowerLevelType {
        use ruma::events::room::power_levels::NotificationPowerLevelType;

        match self {
            Self::Room => NotificationPowerLevelType::Room,
        }
    }
}