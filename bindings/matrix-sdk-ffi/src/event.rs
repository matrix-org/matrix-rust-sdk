use anyhow::{bail, Context};
use ruma::events::{
    room::{message::Relation, redaction::SyncRoomRedactionEvent},
    AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, AnyTimelineEvent,
    MessageLikeEventContent as RumaMessageLikeEventContent, RedactContent,
    RedactedStateEventContent, StaticStateEventContent, SyncMessageLikeEvent, SyncStateEvent,
};

use crate::{
    room_member::MembershipState,
    ruma::{MessageType, NotifyType},
    ClientError,
};

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
    CallNotify { notify_type: NotifyType },
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
    RoomRedaction { redacted_event_id: Option<String>, reason: Option<String> },
    Sticker,
}

impl TryFrom<AnySyncMessageLikeEvent> for MessageLikeEventContent {
    type Error = anyhow::Error;

    fn try_from(value: AnySyncMessageLikeEvent) -> anyhow::Result<Self> {
        let content = match value {
            AnySyncMessageLikeEvent::CallAnswer(_) => MessageLikeEventContent::CallAnswer,
            AnySyncMessageLikeEvent::CallInvite(_) => MessageLikeEventContent::CallInvite,
            AnySyncMessageLikeEvent::CallNotify(content) => {
                let original_content = get_message_like_event_original_content(content)?;
                MessageLikeEventContent::CallNotify {
                    notify_type: original_content.notify_type.into(),
                }
            }
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
                    message_type: original_content.msgtype.into(),
                    in_reply_to_event_id,
                }
            }
            AnySyncMessageLikeEvent::RoomRedaction(c) => {
                let (redacted_event_id, reason) = match c {
                    SyncRoomRedactionEvent::Original(o) => {
                        let id =
                            if o.content.redacts.is_some() { o.content.redacts } else { o.redacts };
                        (id.map(|id| id.to_string()), o.content.reason)
                    }
                    SyncRoomRedactionEvent::Redacted(_) => (None, None),
                };
                MessageLikeEventContent::RoomRedaction { redacted_event_id, reason }
            }
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
    CallCandidates,
    CallHangup,
    CallInvite,
    CallNotify,
    KeyVerificationAccept,
    KeyVerificationCancel,
    KeyVerificationDone,
    KeyVerificationKey,
    KeyVerificationMac,
    KeyVerificationReady,
    KeyVerificationStart,
    PollEnd,
    PollResponse,
    PollStart,
    Reaction,
    RoomEncrypted,
    RoomMessage,
    RoomRedaction,
    Sticker,
    UnstablePollEnd,
    UnstablePollResponse,
    UnstablePollStart,
}

impl From<MessageLikeEventType> for ruma::events::MessageLikeEventType {
    fn from(val: MessageLikeEventType) -> Self {
        match val {
            MessageLikeEventType::CallAnswer => Self::CallAnswer,
            MessageLikeEventType::CallInvite => Self::CallInvite,
            MessageLikeEventType::CallNotify => Self::CallNotify,
            MessageLikeEventType::CallHangup => Self::CallHangup,
            MessageLikeEventType::CallCandidates => Self::CallCandidates,
            MessageLikeEventType::KeyVerificationReady => Self::KeyVerificationReady,
            MessageLikeEventType::KeyVerificationStart => Self::KeyVerificationStart,
            MessageLikeEventType::KeyVerificationCancel => Self::KeyVerificationCancel,
            MessageLikeEventType::KeyVerificationAccept => Self::KeyVerificationAccept,
            MessageLikeEventType::KeyVerificationKey => Self::KeyVerificationKey,
            MessageLikeEventType::KeyVerificationMac => Self::KeyVerificationMac,
            MessageLikeEventType::KeyVerificationDone => Self::KeyVerificationDone,
            MessageLikeEventType::Reaction => Self::Reaction,
            MessageLikeEventType::RoomEncrypted => Self::RoomEncrypted,
            MessageLikeEventType::RoomMessage => Self::RoomMessage,
            MessageLikeEventType::RoomRedaction => Self::RoomRedaction,
            MessageLikeEventType::Sticker => Self::Sticker,
            MessageLikeEventType::PollEnd => Self::PollEnd,
            MessageLikeEventType::PollResponse => Self::PollResponse,
            MessageLikeEventType::PollStart => Self::PollStart,
            MessageLikeEventType::UnstablePollEnd => Self::UnstablePollEnd,
            MessageLikeEventType::UnstablePollResponse => Self::UnstablePollResponse,
            MessageLikeEventType::UnstablePollStart => Self::UnstablePollStart,
        }
    }
}
