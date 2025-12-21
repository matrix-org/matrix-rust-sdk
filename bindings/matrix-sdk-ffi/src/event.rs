use anyhow::{bail, Context};
use matrix_sdk::IdParseError;
use matrix_sdk_ui::timeline::TimelineEventItemId;
use ruma::{
    events::{
        room::{
            encrypted,
            message::{MessageType as RumaMessageType, Relation},
            redaction::SyncRoomRedactionEvent,
        },
        AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, AnyTimelineEvent,
        MessageLikeEventContent as RumaMessageLikeEventContent, RedactContent,
        RedactedStateEventContent, StaticStateEventContent, SyncMessageLikeEvent, SyncStateEvent,
        TimelineEventType as RumaTimelineEventType,
    },
    EventId,
};

use crate::{
    room_member::MembershipState,
    ruma::{MessageType, RtcNotificationType},
    utils::Timestamp,
    ClientError,
};

#[derive(uniffi::Object)]
pub struct TimelineEvent(pub(crate) Box<AnySyncTimelineEvent>);

#[matrix_sdk_ffi_macros::export]
impl TimelineEvent {
    pub fn event_id(&self) -> String {
        self.0.event_id().to_string()
    }

    pub fn sender_id(&self) -> String {
        self.0.sender().to_string()
    }

    pub fn timestamp(&self) -> Timestamp {
        self.0.origin_server_ts().into()
    }

    pub fn content(&self) -> Result<TimelineEventContent, ClientError> {
        let content = match &*self.0 {
            AnySyncTimelineEvent::MessageLike(event) => {
                TimelineEventContent::MessageLike { content: event.clone().try_into()? }
            }
            AnySyncTimelineEvent::State(event) => {
                TimelineEventContent::State { content: event.clone().try_into()? }
            }
        };
        Ok(content)
    }

    /// Returns the thread root event id for the event, if it's part of a
    /// thread.
    pub fn thread_root_event_id(&self) -> Option<String> {
        match &*self.0 {
            AnySyncTimelineEvent::MessageLike(event) => {
                match event.original_content().and_then(|content| content.relation()) {
                    Some(encrypted::Relation::Thread(thread)) => Some(thread.event_id.to_string()),
                    _ => None,
                }
            }
            AnySyncTimelineEvent::State(_) => None,
        }
    }
}

impl From<AnyTimelineEvent> for TimelineEvent {
    fn from(event: AnyTimelineEvent) -> Self {
        Self(Box::new(event.into()))
    }
}

/// The timeline event type.
#[derive(Clone, uniffi::Enum, PartialEq, Eq, Hash)]
pub enum TimelineEventType {
    /// The event is a message-like one and should be displayed as such.
    MessageLike { value: MessageLikeEventType },
    /// The event is a state event, and may or may not be displayed in the
    /// timeline.
    State { value: StateEventType },
}

impl From<RumaTimelineEventType> for TimelineEventType {
    fn from(value: RumaTimelineEventType) -> Self {
        match value {
            RumaTimelineEventType::Audio => {
                Self::MessageLike { value: MessageLikeEventType::Audio }
            }
            RumaTimelineEventType::File => Self::MessageLike { value: MessageLikeEventType::File },
            RumaTimelineEventType::Image => {
                Self::MessageLike { value: MessageLikeEventType::Image }
            }
            RumaTimelineEventType::Video => {
                Self::MessageLike { value: MessageLikeEventType::Video }
            }
            RumaTimelineEventType::Voice => {
                Self::MessageLike { value: MessageLikeEventType::Voice }
            }
            RumaTimelineEventType::Emote => {
                Self::MessageLike { value: MessageLikeEventType::Emote }
            }
            RumaTimelineEventType::Encrypted => {
                Self::MessageLike { value: MessageLikeEventType::Encrypted }
            }
            RumaTimelineEventType::RoomMessage => {
                Self::MessageLike { value: MessageLikeEventType::RoomMessage }
            }
            RumaTimelineEventType::CallAnswer => {
                Self::MessageLike { value: MessageLikeEventType::CallAnswer }
            }
            RumaTimelineEventType::CallInvite => {
                Self::MessageLike { value: MessageLikeEventType::CallInvite }
            }
            RumaTimelineEventType::CallHangup => {
                Self::MessageLike { value: MessageLikeEventType::CallHangup }
            }
            RumaTimelineEventType::CallCandidates => {
                Self::MessageLike { value: MessageLikeEventType::CallCandidates }
            }
            RumaTimelineEventType::CallNegotiate => {
                Self::MessageLike { value: MessageLikeEventType::CallNegotiate }
            }
            RumaTimelineEventType::CallReject => {
                Self::MessageLike { value: MessageLikeEventType::CallReject }
            }
            RumaTimelineEventType::CallSdpStreamMetadataChanged => {
                Self::MessageLike { value: MessageLikeEventType::CallSdpStreamMetadataChanged }
            }
            RumaTimelineEventType::CallSelectAnswer => {
                Self::MessageLike { value: MessageLikeEventType::CallSelectAnswer }
            }
            RumaTimelineEventType::KeyVerificationReady => {
                Self::MessageLike { value: MessageLikeEventType::KeyVerificationReady }
            }
            RumaTimelineEventType::KeyVerificationStart => {
                Self::MessageLike { value: MessageLikeEventType::KeyVerificationStart }
            }
            RumaTimelineEventType::KeyVerificationCancel => {
                Self::MessageLike { value: MessageLikeEventType::KeyVerificationCancel }
            }
            RumaTimelineEventType::KeyVerificationAccept => {
                Self::MessageLike { value: MessageLikeEventType::KeyVerificationAccept }
            }
            RumaTimelineEventType::KeyVerificationKey => {
                Self::MessageLike { value: MessageLikeEventType::KeyVerificationKey }
            }
            RumaTimelineEventType::KeyVerificationMac => {
                Self::MessageLike { value: MessageLikeEventType::KeyVerificationMac }
            }
            RumaTimelineEventType::KeyVerificationDone => {
                Self::MessageLike { value: MessageLikeEventType::KeyVerificationDone }
            }
            RumaTimelineEventType::Location => {
                Self::MessageLike { value: MessageLikeEventType::Location }
            }
            RumaTimelineEventType::Message => {
                Self::MessageLike { value: MessageLikeEventType::Message }
            }
            RumaTimelineEventType::PollStart => {
                Self::MessageLike { value: MessageLikeEventType::PollStart }
            }
            RumaTimelineEventType::UnstablePollStart => {
                Self::MessageLike { value: MessageLikeEventType::UnstablePollStart }
            }
            RumaTimelineEventType::PollResponse => {
                Self::MessageLike { value: MessageLikeEventType::PollResponse }
            }
            RumaTimelineEventType::UnstablePollResponse => {
                Self::MessageLike { value: MessageLikeEventType::UnstablePollResponse }
            }
            RumaTimelineEventType::PollEnd => {
                Self::MessageLike { value: MessageLikeEventType::PollEnd }
            }
            RumaTimelineEventType::UnstablePollEnd => {
                Self::MessageLike { value: MessageLikeEventType::UnstablePollEnd }
            }
            RumaTimelineEventType::Beacon => {
                Self::MessageLike { value: MessageLikeEventType::Beacon }
            }
            RumaTimelineEventType::Reaction => {
                Self::MessageLike { value: MessageLikeEventType::Reaction }
            }
            RumaTimelineEventType::RoomEncrypted => {
                Self::MessageLike { value: MessageLikeEventType::RoomEncrypted }
            }
            RumaTimelineEventType::RoomRedaction => {
                Self::MessageLike { value: MessageLikeEventType::RoomRedaction }
            }
            RumaTimelineEventType::Sticker => {
                Self::MessageLike { value: MessageLikeEventType::Sticker }
            }
            RumaTimelineEventType::CallNotify => {
                Self::MessageLike { value: MessageLikeEventType::CallNotify }
            }
            RumaTimelineEventType::RtcNotification => {
                Self::MessageLike { value: MessageLikeEventType::RtcNotification }
            }
            RumaTimelineEventType::RtcDecline => {
                Self::MessageLike { value: MessageLikeEventType::RtcDecline }
            }
            RumaTimelineEventType::PolicyRuleRoom => {
                Self::State { value: StateEventType::PolicyRuleRoom }
            }
            RumaTimelineEventType::PolicyRuleServer => {
                Self::State { value: StateEventType::PolicyRuleServer }
            }
            RumaTimelineEventType::PolicyRuleUser => {
                Self::State { value: StateEventType::PolicyRuleUser }
            }
            RumaTimelineEventType::RoomAliases => {
                Self::State { value: StateEventType::RoomAliases }
            }
            RumaTimelineEventType::RoomAvatar => Self::State { value: StateEventType::RoomAvatar },
            RumaTimelineEventType::RoomCanonicalAlias => {
                Self::State { value: StateEventType::RoomCanonicalAlias }
            }
            RumaTimelineEventType::RoomCreate => Self::State { value: StateEventType::RoomCreate },
            RumaTimelineEventType::RoomEncryption => {
                Self::State { value: StateEventType::RoomEncryption }
            }
            RumaTimelineEventType::RoomGuestAccess => {
                Self::State { value: StateEventType::RoomGuestAccess }
            }
            RumaTimelineEventType::RoomHistoryVisibility => {
                Self::State { value: StateEventType::RoomHistoryVisibility }
            }
            RumaTimelineEventType::RoomJoinRules => {
                Self::State { value: StateEventType::RoomJoinRules }
            }
            RumaTimelineEventType::RoomMember => {
                Self::State { value: StateEventType::RoomMemberEvent }
            }
            RumaTimelineEventType::RoomLanguage => {
                Self::State { value: StateEventType::RoomLanguage }
            }
            RumaTimelineEventType::RoomName => Self::State { value: StateEventType::RoomName },
            RumaTimelineEventType::RoomImagePack => {
                Self::State { value: StateEventType::RoomImagePack }
            }
            RumaTimelineEventType::RoomPinnedEvents => {
                Self::State { value: StateEventType::RoomPinnedEvents }
            }
            RumaTimelineEventType::RoomPowerLevels => {
                Self::State { value: StateEventType::RoomPowerLevels }
            }
            RumaTimelineEventType::RoomServerAcl => {
                Self::State { value: StateEventType::RoomServerAcl }
            }
            RumaTimelineEventType::RoomThirdPartyInvite => {
                Self::State { value: StateEventType::RoomThirdPartyInvite }
            }
            RumaTimelineEventType::RoomTombstone => {
                Self::State { value: StateEventType::RoomTombstone }
            }
            RumaTimelineEventType::RoomTopic => Self::State { value: StateEventType::RoomTopic },
            RumaTimelineEventType::SpaceChild => Self::State { value: StateEventType::SpaceChild },
            RumaTimelineEventType::SpaceParent => {
                Self::State { value: StateEventType::SpaceParent }
            }
            RumaTimelineEventType::BeaconInfo => Self::State { value: StateEventType::BeaconInfo },
            RumaTimelineEventType::CallMember => Self::State { value: StateEventType::CallMember },
            RumaTimelineEventType::MemberHints => {
                Self::State { value: StateEventType::MemberHints }
            }
            RumaTimelineEventType::_Custom(_) => {
                Self::State { value: StateEventType::Custom { value: value.to_string() } }
            }
            _ => Self::MessageLike { value: MessageLikeEventType::Other(value.to_string()) },
        }
    }
}

#[derive(uniffi::Enum)]
// A note about this `allow(clippy::large_enum_variant)`.
// In order to reduce the size of `TimelineEventContent`, we would need to
// put some parts in a `Box`, or an `Arc`. Sadly, it doesn't play well with
// UniFFI. We would need to change the `uniffi::Record` of the subtypes into
// `uniffi::Object`, which is a radical change. It would simplify the memory
// usage, but it would slow down the performance around the FFI border. Thus,
// let's consider this is a false-positive lint in this particular case.
#[allow(clippy::large_enum_variant)]
pub enum TimelineEventContent {
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
    RoomTopic { topic: String },
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
                    membership_state: original_content.membership.try_into()?,
                }
            }
            AnySyncStateEvent::RoomName(_) => StateEventContent::RoomName,
            AnySyncStateEvent::RoomPinnedEvents(_) => StateEventContent::RoomPinnedEvents,
            AnySyncStateEvent::RoomPowerLevels(_) => StateEventContent::RoomPowerLevels,
            AnySyncStateEvent::RoomServerAcl(_) => StateEventContent::RoomServerAcl,
            AnySyncStateEvent::RoomThirdPartyInvite(_) => StateEventContent::RoomThirdPartyInvite,
            AnySyncStateEvent::RoomTombstone(_) => StateEventContent::RoomTombstone,
            AnySyncStateEvent::RoomTopic(content) => {
                let content = get_state_event_original_content(content)?;

                StateEventContent::RoomTopic { topic: content.topic }
            }
            AnySyncStateEvent::SpaceChild(_) => StateEventContent::SpaceChild,
            AnySyncStateEvent::SpaceParent(_) => StateEventContent::SpaceParent,
            _ => bail!("Unsupported state event: {:?}", value.event_type()),
        };
        Ok(event)
    }
}

#[derive(uniffi::Enum)]
// A note about this `allow(clippy::large_enum_variant)`.
// In order to reduce the size of `MessageLineEventContent`, we would need to
// put some parts in a `Box`, or an `Arc`. Sadly, it doesn't play well with
// UniFFI. We would need to change the `uniffi::Record` of the subtypes into
// `uniffi::Object`, which is a radical change. It would simplify the memory
// usage, but it would slow down the performance around the FFI border. Thus,
// let's consider this is a false-positive lint in this particular case.
#[allow(clippy::large_enum_variant)]
pub enum MessageLikeEventContent {
    CallAnswer,
    CallInvite,
    RtcNotification {
        notification_type: RtcNotificationType,
        /// The timestamp at which this notification is considered invalid.
        expiration_ts: Timestamp,
    },
    CallHangup,
    CallCandidates,
    KeyVerificationReady,
    KeyVerificationStart,
    KeyVerificationCancel,
    KeyVerificationAccept,
    KeyVerificationKey,
    KeyVerificationMac,
    KeyVerificationDone,
    Poll {
        question: String,
    },
    ReactionContent {
        related_event_id: String,
    },
    RoomEncrypted,
    RoomMessage {
        message_type: MessageType,
        in_reply_to_event_id: Option<String>,
    },
    RoomRedaction {
        redacted_event_id: Option<String>,
        reason: Option<String>,
    },
    Sticker,
}

impl TryFrom<AnySyncMessageLikeEvent> for MessageLikeEventContent {
    type Error = anyhow::Error;

    fn try_from(value: AnySyncMessageLikeEvent) -> anyhow::Result<Self> {
        let content = match value {
            AnySyncMessageLikeEvent::CallAnswer(_) => MessageLikeEventContent::CallAnswer,
            AnySyncMessageLikeEvent::CallInvite(_) => MessageLikeEventContent::CallInvite,
            AnySyncMessageLikeEvent::RtcNotification(event) => {
                let origin_server_ts = event.origin_server_ts();
                let original_content = get_message_like_event_original_content(event)?;
                let expiration_ts = original_content.expiration_ts(origin_server_ts, None).into();
                MessageLikeEventContent::RtcNotification {
                    notification_type: original_content.notification_type.into(),
                    expiration_ts,
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
                    message_type: original_content.msgtype.try_into()?,
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
            _ => bail!("Unsupported Event Type: {:?}", value.event_type()),
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

#[derive(Clone, uniffi::Enum, PartialEq, Eq, Hash)]
pub enum StateEventType {
    BeaconInfo,
    CallMember,
    MemberHints,
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
    RoomImagePack,
    RoomJoinRules,
    RoomMemberEvent,
    RoomLanguage,
    RoomName,
    RoomPinnedEvents,
    RoomPowerLevels,
    RoomServerAcl,
    RoomThirdPartyInvite,
    RoomTombstone,
    RoomTopic,
    SpaceChild,
    SpaceParent,
    Custom { value: String },
}

impl From<StateEventType> for ruma::events::StateEventType {
    fn from(val: StateEventType) -> Self {
        match val {
            StateEventType::BeaconInfo => Self::BeaconInfo,
            StateEventType::CallMember => Self::CallMember,
            StateEventType::MemberHints => Self::MemberHints,
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
            StateEventType::RoomImagePack => Self::RoomImagePack,
            StateEventType::RoomJoinRules => Self::RoomJoinRules,
            StateEventType::RoomLanguage => Self::RoomLanguage,
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
            StateEventType::Custom { value } => value.into(),
        }
    }
}

#[derive(Clone, uniffi::Enum, PartialEq, Eq, Hash)]
pub enum MessageLikeEventType {
    Audio,
    Beacon,
    CallAnswer,
    CallCandidates,
    CallHangup,
    CallInvite,
    CallNegotiate,
    CallNotify,
    CallReject,
    CallSdpStreamMetadataChanged,
    CallSelectAnswer,
    Emote,
    Encrypted,
    File,
    Image,
    KeyVerificationAccept,
    KeyVerificationCancel,
    KeyVerificationDone,
    KeyVerificationKey,
    KeyVerificationMac,
    KeyVerificationReady,
    KeyVerificationStart,
    Location,
    Message,
    PollEnd,
    PollResponse,
    PollStart,
    Reaction,
    RoomEncrypted,
    RoomMessage,
    RoomRedaction,
    RtcDecline,
    RtcNotification,
    Sticker,
    UnstablePollEnd,
    UnstablePollResponse,
    UnstablePollStart,
    Video,
    Voice,
    Other(String),
}

impl From<MessageLikeEventType> for ruma::events::MessageLikeEventType {
    fn from(val: MessageLikeEventType) -> Self {
        match val {
            MessageLikeEventType::Audio => Self::Audio,
            MessageLikeEventType::File => Self::File,
            MessageLikeEventType::Image => Self::Image,
            MessageLikeEventType::Video => Self::Video,
            MessageLikeEventType::Voice => Self::Voice,
            MessageLikeEventType::Beacon => Self::Beacon,
            MessageLikeEventType::CallAnswer => Self::CallAnswer,
            MessageLikeEventType::CallCandidates => Self::CallCandidates,
            MessageLikeEventType::CallInvite => Self::CallInvite,
            MessageLikeEventType::CallHangup => Self::CallHangup,
            MessageLikeEventType::CallNegotiate => Self::CallNegotiate,
            MessageLikeEventType::CallNotify => Self::CallNotify,
            MessageLikeEventType::CallReject => Self::CallReject,
            MessageLikeEventType::CallSdpStreamMetadataChanged => {
                Self::CallSdpStreamMetadataChanged
            }
            MessageLikeEventType::CallSelectAnswer => Self::CallSelectAnswer,
            MessageLikeEventType::Emote => Self::Emote,
            MessageLikeEventType::Encrypted => Self::Encrypted,
            MessageLikeEventType::KeyVerificationReady => Self::KeyVerificationReady,
            MessageLikeEventType::KeyVerificationStart => Self::KeyVerificationStart,
            MessageLikeEventType::KeyVerificationCancel => Self::KeyVerificationCancel,
            MessageLikeEventType::KeyVerificationAccept => Self::KeyVerificationAccept,
            MessageLikeEventType::KeyVerificationKey => Self::KeyVerificationKey,
            MessageLikeEventType::KeyVerificationMac => Self::KeyVerificationMac,
            MessageLikeEventType::KeyVerificationDone => Self::KeyVerificationDone,
            MessageLikeEventType::Location => Self::Location,
            MessageLikeEventType::Message => Self::Message,
            MessageLikeEventType::Reaction => Self::Reaction,
            MessageLikeEventType::RoomEncrypted => Self::RoomEncrypted,
            MessageLikeEventType::RoomMessage => Self::RoomMessage,
            MessageLikeEventType::RoomRedaction => Self::RoomRedaction,
            MessageLikeEventType::RtcDecline => Self::RtcDecline,
            MessageLikeEventType::Sticker => Self::Sticker,
            MessageLikeEventType::PollEnd => Self::PollEnd,
            MessageLikeEventType::PollResponse => Self::PollResponse,
            MessageLikeEventType::PollStart => Self::PollStart,
            MessageLikeEventType::RtcNotification => Self::RtcNotification,
            MessageLikeEventType::UnstablePollEnd => Self::UnstablePollEnd,
            MessageLikeEventType::UnstablePollResponse => Self::UnstablePollResponse,
            MessageLikeEventType::UnstablePollStart => Self::UnstablePollStart,
            MessageLikeEventType::Other(msgtype) => Self::from(msgtype),
        }
    }
}

#[derive(Debug, PartialEq, Clone, uniffi::Enum)]
pub enum RoomMessageEventMessageType {
    Audio,
    Emote,
    File,
    #[cfg(feature = "unstable-msc4274")]
    Gallery,
    Image,
    Location,
    Notice,
    ServerNotice,
    Text,
    Video,
    VerificationRequest,
    Other,
}

impl From<RumaMessageType> for RoomMessageEventMessageType {
    fn from(val: ruma::events::room::message::MessageType) -> Self {
        match val {
            RumaMessageType::Audio { .. } => Self::Audio,
            RumaMessageType::Emote { .. } => Self::Emote,
            RumaMessageType::File { .. } => Self::File,
            #[cfg(feature = "unstable-msc4274")]
            RumaMessageType::Gallery { .. } => Self::Gallery,
            RumaMessageType::Image { .. } => Self::Image,
            RumaMessageType::Location { .. } => Self::Location,
            RumaMessageType::Notice { .. } => Self::Notice,
            RumaMessageType::ServerNotice { .. } => Self::ServerNotice,
            RumaMessageType::Text { .. } => Self::Text,
            RumaMessageType::Video { .. } => Self::Video,
            RumaMessageType::VerificationRequest { .. } => Self::VerificationRequest,
            _ => Self::Other,
        }
    }
}

/// Contains the 2 possible identifiers of an event, either it has a remote
/// event id or a local transaction id, never both or none.
#[derive(Clone, uniffi::Enum)]
pub enum EventOrTransactionId {
    EventId { event_id: String },
    TransactionId { transaction_id: String },
}

impl From<TimelineEventItemId> for EventOrTransactionId {
    fn from(value: TimelineEventItemId) -> Self {
        match value {
            TimelineEventItemId::EventId(event_id) => {
                EventOrTransactionId::EventId { event_id: event_id.to_string() }
            }
            TimelineEventItemId::TransactionId(transaction_id) => {
                EventOrTransactionId::TransactionId { transaction_id: transaction_id.to_string() }
            }
        }
    }
}

impl TryFrom<EventOrTransactionId> for TimelineEventItemId {
    type Error = IdParseError;
    fn try_from(value: EventOrTransactionId) -> Result<Self, Self::Error> {
        match value {
            EventOrTransactionId::EventId { event_id } => {
                Ok(TimelineEventItemId::EventId(EventId::parse(event_id)?))
            }
            EventOrTransactionId::TransactionId { transaction_id } => {
                Ok(TimelineEventItemId::TransactionId(transaction_id.into()))
            }
        }
    }
}
