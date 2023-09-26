use anyhow::{bail, Context};
use ruma::events::{
    room::message::Relation, AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent,
    AnyTimelineEvent, MessageLikeEventContent as RumaMessageLikeEventContent, RedactContent,
    RedactedStateEventContent, StaticStateEventContent, SyncMessageLikeEvent, SyncStateEvent,
};

use crate::{room_member::MembershipState, timeline::MessageType, ClientError};

#[derive(uniffi::Object)]
pub struct TimelineEvent(pub(crate) AnySyncTimelineEvent);

#[uniffi::export]
impl TimelineEvent {
    pub fn event_id(&self) -> String {
        self.0.event_id().to_string()
    }

    pub fn sender_id(&self) -> String {
        self.0.sender().to_string()
    }

    pub fn timestamp(&self) -> u64 {
        self.0.origin_server_ts().0.into()
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

impl From<AnyTimelineEvent> for TimelineEvent {
    fn from(event: AnyTimelineEvent) -> Self {
        Self(event.into())
    }
}

#[derive(uniffi::Enum)]
pub enum TimelineEventType {
    MessageLike { content: MessageLikeEventContent },
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
    RoomMemberContent { user_id: String, membership_state: MembershipState },
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
                let original_content = get_state_event_original_content(content)?;
                StateEventContent::RoomMemberContent {
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
    Poll { question: String },
    ReactionContent { related_event_id: String },
    RoomEncrypted,
    RoomMessage { message_type: MessageType, in_reply_to_event_id: Option<String> },
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
            AnySyncMessageLikeEvent::UnstablePollStart(content) => {
                let original_content = get_message_like_event_original_content(content)?;
                MessageLikeEventContent::Poll {
                    question: original_content.poll_start().question.text.clone(),
                }
            }
            AnySyncMessageLikeEvent::Reaction(content) => {
                let original_content = get_message_like_event_original_content(content)?;
                MessageLikeEventContent::ReactionContent {
                    related_event_id: original_content.relates_to.event_id.to_string(),
                }
            }
            AnySyncMessageLikeEvent::RoomEncrypted(_) => MessageLikeEventContent::RoomEncrypted,
            AnySyncMessageLikeEvent::RoomMessage(content) => {
                let original_content = get_message_like_event_original_content(content)?;
                let in_reply_to_event_id =
                    original_content.relates_to.and_then(|relation| match relation {
                        Relation::Reply { in_reply_to } => Some(in_reply_to.event_id.to_string()),
                        _ => None,
                    });
                MessageLikeEventContent::RoomMessage {
                    message_type: original_content.msgtype.try_into()?,
                    in_reply_to_event_id,
                }
            }
            AnySyncMessageLikeEvent::RoomRedaction(_) => MessageLikeEventContent::RoomRedaction,
            AnySyncMessageLikeEvent::Sticker(_) => MessageLikeEventContent::Sticker,
            _ => bail!("Unsupported Event Type"),
        };
        Ok(content)
    }
}

fn get_state_event_original_content<C>(event: SyncStateEvent<C>) -> anyhow::Result<C>
where
    C: StaticStateEventContent + RedactContent + Clone,
    <C as RedactContent>::Redacted: RedactedStateEventContent<StateKey = C::StateKey>,
{
    let original_content =
        event.as_original().context("Failed to get original content")?.content.clone();
    Ok(original_content)
}

fn get_message_like_event_original_content<C>(event: SyncMessageLikeEvent<C>) -> anyhow::Result<C>
where
    C: RumaMessageLikeEventContent + RedactContent + Clone,
    <C as ruma::events::RedactContent>::Redacted: ruma::events::RedactedMessageLikeEventContent,
{
    let original_content =
        event.as_original().context("Failed to get original content")?.content.clone();
    Ok(original_content)
}
