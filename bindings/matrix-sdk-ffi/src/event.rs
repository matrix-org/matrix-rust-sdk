use anyhow::{bail, Context};
use ruma::events::{AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent};

use crate::{ClientError, MembershipState, MessageType};

pub struct TimelineEvent(pub(crate) AnySyncTimelineEvent);

#[uniffi::export]
impl TimelineEvent {
    pub fn event_id(&self) -> String {
        self.0.event_id().to_string()
    }

    pub fn sender_id(&self) -> String {
        self.0.sender().to_string()
    }

    pub fn event_type(&self) -> Result<TimelineEventType, ClientError> {
        let event_type = match &self.0 {
            AnySyncTimelineEvent::MessageLike(event) => {
                TimelineEventType::MessageLike { content: event.clone().try_into()? }
            }
            AnySyncTimelineEvent::State(event) => {
                TimelineEventType::State { content: event.clone().try_into()? }
            }
        };
        Ok(event_type)
    }
}

#[derive(uniffi::Enum)]
pub enum TimelineEventType {
    MessageLike { content: MessageLikeEventContent },
    // we don't support state events yet so they do not have any content
    State { content: StateEventContent },
}

#[derive(uniffi::Enum)]
pub enum StateEventContent {
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
    RoomMember { user_id: String, membership_state: MembershipState },
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

impl TryFrom<AnySyncStateEvent> for StateEventContent {
    type Error = anyhow::Error;

    fn try_from(value: AnySyncStateEvent) -> anyhow::Result<Self> {
        let event = match value {
            AnySyncStateEvent::PolicyRuleRoom(_) => StateEventContent::PolicyRuleRoom,
            AnySyncStateEvent::PolicyRuleServer(_) => StateEventContent::PolicyRuleServer,
            AnySyncStateEvent::PolicyRuleUser(_) => StateEventContent::PolicyRuleUser,
            AnySyncStateEvent::RoomAliases(_) => StateEventContent::RoomAliases,
            AnySyncStateEvent::RoomAvatar(_) => StateEventContent::RoomAvatar,
            AnySyncStateEvent::RoomCanonicalAlias(_) => StateEventContent::RoomCanonicalAlias,
            AnySyncStateEvent::RoomCreate(_) => StateEventContent::RoomCreate,
            AnySyncStateEvent::RoomEncryption(_) => StateEventContent::RoomEncryption,
            AnySyncStateEvent::RoomGuestAccess(_) => StateEventContent::RoomGuestAccess,
            AnySyncStateEvent::RoomHistoryVisibility(_) => StateEventContent::RoomHistoryVisibility,
            AnySyncStateEvent::RoomJoinRules(_) => StateEventContent::RoomJoinRules,
            AnySyncStateEvent::RoomMember(content) => {
                let state_key = content.state_key().to_string();
                let original_content = content
                    .as_original()
                    .context("Failed to get original content")?
                    .content
                    .clone();
                StateEventContent::RoomMember {
                    user_id: state_key,
                    membership_state: original_content.membership.into(),
                }
            }
            AnySyncStateEvent::RoomName(_) => StateEventContent::RoomName,
            AnySyncStateEvent::RoomPinnedEvents(_) => StateEventContent::RoomPinnedEvents,
            AnySyncStateEvent::RoomPowerLevels(_) => StateEventContent::RoomPowerLevels,
            AnySyncStateEvent::RoomServerAcl(_) => StateEventContent::RoomServerAcl,
            AnySyncStateEvent::RoomThirdPartyInvite(_) => StateEventContent::RoomThirdPartyInvite,
            AnySyncStateEvent::RoomTombstone(_) => StateEventContent::RoomTombstone,
            AnySyncStateEvent::RoomTopic(_) => StateEventContent::RoomTopic,
            AnySyncStateEvent::SpaceChild(_) => StateEventContent::SpaceChild,
            AnySyncStateEvent::SpaceParent(_) => StateEventContent::SpaceParent,
            _ => bail!("Unsupported state event"),
        };
        Ok(event)
    }
}

#[derive(uniffi::Enum)]
pub enum MessageLikeEventContent {
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
    ReactionContent { related_event_id: String },
    RoomEncrypted,
    RoomMessage { message_type: MessageType },
    RoomRedaction,
    Sticker,
}

impl TryFrom<AnySyncMessageLikeEvent> for MessageLikeEventContent {
    type Error = anyhow::Error;

    fn try_from(value: AnySyncMessageLikeEvent) -> anyhow::Result<Self> {
        let content = match value {
            AnySyncMessageLikeEvent::CallAnswer(_) => MessageLikeEventContent::CallAnswer,
            AnySyncMessageLikeEvent::CallInvite(_) => MessageLikeEventContent::CallInvite,
            AnySyncMessageLikeEvent::CallHangup(_) => MessageLikeEventContent::CallHangup,
            AnySyncMessageLikeEvent::CallCandidates(_) => MessageLikeEventContent::CallCandidates,
            AnySyncMessageLikeEvent::KeyVerificationReady(_) => {
                MessageLikeEventContent::KeyVerificationReady
            }
            AnySyncMessageLikeEvent::KeyVerificationStart(_) => {
                MessageLikeEventContent::KeyVerificationStart
            }
            AnySyncMessageLikeEvent::KeyVerificationCancel(_) => {
                MessageLikeEventContent::KeyVerificationCancel
            }
            AnySyncMessageLikeEvent::KeyVerificationAccept(_) => {
                MessageLikeEventContent::KeyVerificationAccept
            }
            AnySyncMessageLikeEvent::KeyVerificationKey(_) => {
                MessageLikeEventContent::KeyVerificationKey
            }
            AnySyncMessageLikeEvent::KeyVerificationMac(_) => {
                MessageLikeEventContent::KeyVerificationMac
            }
            AnySyncMessageLikeEvent::KeyVerificationDone(_) => {
                MessageLikeEventContent::KeyVerificationDone
            }
            AnySyncMessageLikeEvent::Reaction(content) => {
                let original_content = content
                    .as_original()
                    .context("Failed to get original content")?
                    .content
                    .clone();
                MessageLikeEventContent::ReactionContent {
                    related_event_id: original_content.relates_to.event_id.to_string(),
                }
            }
            AnySyncMessageLikeEvent::RoomEncrypted(_) => MessageLikeEventContent::RoomEncrypted,
            AnySyncMessageLikeEvent::RoomMessage(content) => {
                let original_content = content
                    .as_original()
                    .context("Failed to get original content")?
                    .content
                    .clone();
                MessageLikeEventContent::RoomMessage {
                    message_type: original_content.msgtype.try_into()?,
                }
            }
            AnySyncMessageLikeEvent::RoomRedaction(_) => MessageLikeEventContent::RoomRedaction,
            AnySyncMessageLikeEvent::Sticker(_) => MessageLikeEventContent::Sticker,
            _ => bail!("Unsupported Event Type"),
        };
        Ok(content)
    }
}
