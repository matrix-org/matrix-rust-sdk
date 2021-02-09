use std::time::SystemTime;

use crate::{
    events::{
        call::{
            answer::AnswerEventContent, candidates::CandidatesEventContent,
            hangup::HangupEventContent, invite::InviteEventContent,
        },
        custom::CustomEventContent as RumaCustomEventContent,
        key::verification::{
            accept::AcceptEventContent, cancel::CancelEventContent, done::DoneEventContent,
            key::KeyEventContent, mac::MacEventContent, ready::ReadyEventContent,
            start::StartEventContent,
        },
        reaction::ReactionEventContent,
        room::{
            encrypted::EncryptedEventContent,
            message::{feedback::FeedbackEventContent, MessageEventContent},
            redaction::{RedactionEventContent, SyncRedactionEvent as RumaSyncRedationEvent},
        },
        sticker::StickerEventContent,
        AnyRedactedSyncMessageEvent, AnyRedactedSyncStateEvent,
        AnySyncMessageEvent as RumaAnySyncMessageEvent, AnySyncRoomEvent as RumaSyncRoomEvent,
        AnySyncStateEvent, MessageEventContent as MessageEventContentTrait,
        SyncMessageEvent as RumaSyncMessageEvent, Unsigned,
    },
    identifiers::{DeviceIdBox, EventId, UserId},
};
use ruma::events::{AnyMessageEventContent, EventContent, RoomEventContent};
use serde_json::{value::RawValue as RawJsonValue, Value as JsonValue};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum VerificationState {
    Trusted,
    Untrusted,
    UnknownDevice,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncryptionInfo {
    pub sender: UserId,
    pub sender_device: DeviceIdBox,
    pub verification_state: VerificationState,
}

/// A custom event's type and `content` JSON object.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomEventContent {
    /// The event type string.
    #[serde(skip)]
    pub event_type: String,

    /// The actual `content` JSON object.
    #[serde(flatten)]
    pub json: JsonValue,
}

impl From<RumaCustomEventContent> for CustomEventContent {
    fn from(e: RumaCustomEventContent) -> Self {
        Self {
            event_type: e.event_type,
            json: e.json,
        }
    }
}

impl EventContent for CustomEventContent {
    fn event_type(&self) -> &str {
        &self.event_type
    }

    fn from_parts(event_type: &str, content: Box<RawJsonValue>) -> Result<Self, serde_json::Error> {
        let json = serde_json::from_str(content.get())?;
        Ok(Self {
            event_type: event_type.to_string(),
            json,
        })
    }
}

impl Into<RumaCustomEventContent> for CustomEventContent {
    fn into(self) -> RumaCustomEventContent {
        RumaCustomEventContent {
            event_type: self.event_type,
            json: self.json,
        }
    }
}

impl RoomEventContent for CustomEventContent {}
impl MessageEventContentTrait for CustomEventContent {}

/// Any sync room event (room event without a `room_id`, as returned in `/sync` responses)
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum AnySyncRoomEvent {
    /// Any sync message event
    Message(AnySyncMessageEvent),

    /// Any sync state event
    State(AnySyncStateEvent),

    /// Any sync message event that has been redacted.
    RedactedMessage(AnyRedactedSyncMessageEvent),

    /// Any sync state event that has been redacted.
    RedactedState(AnyRedactedSyncStateEvent),
}

/// A message event without a `room_id`.
///
/// `SyncMessageEvent` implements the comparison traits using only
/// the `event_id` field, a sorted list would be sorted lexicographically based on
/// the event's `EventId`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncMessageEvent<C: MessageEventContentTrait> {
    /// Data specific to the event type.
    pub content: C,

    /// The globally unique event identifier for the user who sent the event.
    pub event_id: EventId,

    /// The fully-qualified ID of the user who sent this event.
    pub sender: UserId,

    /// Timestamp in milliseconds on originating homeserver when this event was sent.
    pub origin_server_ts: SystemTime,

    /// Additional key-value pairs not signed by the homeserver.
    pub unsigned: Unsigned,

    /// Information about the encryption state of the event.
    ///
    /// Will be `None` if the event wasn't encrypted.
    pub encryption_info: Option<EncryptionInfo>,
}

/// Redaction event without a `room_id`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncRedactionEvent {
    /// Data specific to the event type.
    pub content: RedactionEventContent,

    /// The ID of the event that was redacted.
    pub redacts: EventId,

    /// The globally unique event identifier for the user who sent the event.
    pub event_id: EventId,

    /// The fully-qualified ID of the user who sent this event.
    pub sender: UserId,

    /// Timestamp in milliseconds on originating homeserver when this event was sent.
    pub origin_server_ts: SystemTime,

    /// Additional key-value pairs not signed by the homeserver.
    pub unsigned: Unsigned,

    /// Information about the encryption state of the event.
    ///
    /// Will be `None` if the event wasn't encrypted.
    pub encryption_info: Option<EncryptionInfo>,
}

impl<C: MessageEventContentTrait> SyncMessageEvent<C> {
    pub fn from_ruma(
        event: RumaSyncMessageEvent<C>,
        encryption_info: Option<EncryptionInfo>,
    ) -> SyncMessageEvent<C> {
        Self {
            content: event.content,
            event_id: event.event_id,
            sender: event.sender,
            origin_server_ts: event.origin_server_ts,
            unsigned: event.unsigned,
            encryption_info,
        }
    }
}

impl SyncRedactionEvent {
    pub fn from_ruma_redaction(
        event: RumaSyncRedationEvent,
        encryption_info: Option<EncryptionInfo>,
    ) -> SyncRedactionEvent {
        Self {
            content: event.content,
            event_id: event.event_id,
            sender: event.sender,
            origin_server_ts: event.origin_server_ts,
            unsigned: event.unsigned,
            encryption_info,
            redacts: event.redacts,
        }
    }
}

impl From<RumaSyncMessageEvent<RumaCustomEventContent>> for SyncMessageEvent<CustomEventContent> {
    fn from(e: RumaSyncMessageEvent<RumaCustomEventContent>) -> Self {
        Self {
            content: e.content.into(),
            event_id: e.event_id,
            sender: e.sender,
            origin_server_ts: e.origin_server_ts,
            unsigned: e.unsigned,
            encryption_info: None,
        }
    }
}

/// Enum holding the different event types a sync timeline can hold.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum AnySyncMessageEvent {
    RoomMessage(SyncMessageEvent<MessageEventContent>),
    CallAnswer(SyncMessageEvent<AnswerEventContent>),
    CallInvite(SyncMessageEvent<InviteEventContent>),
    CallHangup(SyncMessageEvent<HangupEventContent>),
    CallCandidates(SyncMessageEvent<CandidatesEventContent>),
    KeyVerificationReady(SyncMessageEvent<ReadyEventContent>),
    KeyVerificationStart(SyncMessageEvent<StartEventContent>),
    KeyVerificationCancel(SyncMessageEvent<CancelEventContent>),
    KeyVerificationAccept(SyncMessageEvent<AcceptEventContent>),
    KeyVerificationKey(SyncMessageEvent<KeyEventContent>),
    KeyVerificationMac(SyncMessageEvent<MacEventContent>),
    KeyVerificationDone(SyncMessageEvent<DoneEventContent>),
    Reaction(SyncMessageEvent<ReactionEventContent>),
    RoomEncrypted(SyncMessageEvent<EncryptedEventContent>),
    RoomMessageFeedback(SyncMessageEvent<FeedbackEventContent>),
    RoomRedaction(SyncRedactionEvent),
    Sticker(SyncMessageEvent<StickerEventContent>),
    Custom(SyncMessageEvent<CustomEventContent>),
}

macro_rules! fromEvent {
    ($enum_type:ident, $event_type:ident) => {
        impl From<SyncMessageEvent<$event_type>> for AnySyncMessageEvent {
            fn from(e: SyncMessageEvent<$event_type>) -> Self {
                Self::$enum_type(e)
            }
        }
    };
}

fromEvent!(RoomMessage, MessageEventContent);
fromEvent!(CallAnswer, AnswerEventContent);
fromEvent!(CallInvite, InviteEventContent);
fromEvent!(CallHangup, HangupEventContent);
fromEvent!(CallCandidates, CandidatesEventContent);
fromEvent!(KeyVerificationReady, ReadyEventContent);
fromEvent!(KeyVerificationStart, StartEventContent);
fromEvent!(KeyVerificationCancel, CancelEventContent);
fromEvent!(KeyVerificationAccept, AcceptEventContent);
fromEvent!(KeyVerificationKey, KeyEventContent);
fromEvent!(KeyVerificationMac, MacEventContent);
fromEvent!(KeyVerificationDone, DoneEventContent);
fromEvent!(Reaction, ReactionEventContent);
fromEvent!(RoomEncrypted, EncryptedEventContent);
fromEvent!(RoomMessageFeedback, FeedbackEventContent);
fromEvent!(Sticker, StickerEventContent);
fromEvent!(Custom, CustomEventContent);

impl<C: MessageEventContentTrait> From<RumaSyncMessageEvent<C>> for SyncMessageEvent<C> {
    fn from(e: RumaSyncMessageEvent<C>) -> Self {
        Self {
            content: e.content,
            event_id: e.event_id,
            sender: e.sender,
            origin_server_ts: e.origin_server_ts,
            unsigned: e.unsigned,
            encryption_info: None,
        }
    }
}

impl From<SyncRedactionEvent> for AnySyncMessageEvent {
    fn from(e: SyncRedactionEvent) -> Self {
        Self::RoomRedaction(e)
    }
}

impl AnySyncMessageEvent {
    pub fn from_ruma(event: RumaAnySyncMessageEvent, info: Option<EncryptionInfo>) -> Self {
        match event {
            RumaAnySyncMessageEvent::CallAnswer(e) => SyncMessageEvent::from_ruma(e, info).into(),
            RumaAnySyncMessageEvent::CallInvite(e) => SyncMessageEvent::from_ruma(e, info).into(),
            RumaAnySyncMessageEvent::CallHangup(e) => SyncMessageEvent::from_ruma(e, info).into(),
            RumaAnySyncMessageEvent::CallCandidates(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::KeyVerificationReady(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::KeyVerificationStart(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::KeyVerificationCancel(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::KeyVerificationAccept(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::KeyVerificationKey(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::KeyVerificationMac(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::KeyVerificationDone(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::Reaction(e) => SyncMessageEvent::from_ruma(e, info).into(),
            RumaAnySyncMessageEvent::RoomEncrypted(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::RoomMessage(e) => SyncMessageEvent::from_ruma(e, info).into(),
            RumaAnySyncMessageEvent::RoomMessageFeedback(e) => {
                SyncMessageEvent::from_ruma(e, info).into()
            }
            RumaAnySyncMessageEvent::RoomRedaction(e) => {
                SyncRedactionEvent::from_ruma_redaction(e, info).into()
            }
            RumaAnySyncMessageEvent::Sticker(e) => SyncMessageEvent::from_ruma(e, info).into(),
            RumaAnySyncMessageEvent::Custom(e) => {
                let event: SyncMessageEvent<CustomEventContent> = e.into();
                AnySyncMessageEvent::Custom(event)
            }
        }
    }
}

impl AnySyncMessageEvent {
    pub fn sender(&self) -> &UserId {
        match self {
            AnySyncMessageEvent::RoomMessage(e) => &e.sender,
            AnySyncMessageEvent::CallAnswer(e) => &e.sender,
            AnySyncMessageEvent::CallInvite(e) => &e.sender,
            AnySyncMessageEvent::CallHangup(e) => &e.sender,
            AnySyncMessageEvent::CallCandidates(e) => &e.sender,
            AnySyncMessageEvent::KeyVerificationReady(e) => &e.sender,
            AnySyncMessageEvent::KeyVerificationStart(e) => &e.sender,
            AnySyncMessageEvent::KeyVerificationCancel(e) => &e.sender,
            AnySyncMessageEvent::KeyVerificationAccept(e) => &e.sender,
            AnySyncMessageEvent::KeyVerificationKey(e) => &e.sender,
            AnySyncMessageEvent::KeyVerificationMac(e) => &e.sender,
            AnySyncMessageEvent::KeyVerificationDone(e) => &e.sender,
            AnySyncMessageEvent::Reaction(e) => &e.sender,
            AnySyncMessageEvent::RoomEncrypted(e) => &e.sender,
            AnySyncMessageEvent::RoomMessageFeedback(e) => &e.sender,
            AnySyncMessageEvent::RoomRedaction(e) => &e.sender,
            AnySyncMessageEvent::Sticker(e) => &e.sender,
            AnySyncMessageEvent::Custom(e) => &e.sender,
        }
    }

    pub fn content(&self) -> AnyMessageEventContent {
        match self {
            AnySyncMessageEvent::RoomMessage(e) => {
                AnyMessageEventContent::RoomMessage(e.content.clone())
            }
            AnySyncMessageEvent::CallAnswer(e) => {
                AnyMessageEventContent::CallAnswer(e.content.clone())
            }
            AnySyncMessageEvent::CallInvite(e) => {
                AnyMessageEventContent::CallInvite(e.content.clone())
            }
            AnySyncMessageEvent::CallHangup(e) => {
                AnyMessageEventContent::CallHangup(e.content.clone())
            }
            AnySyncMessageEvent::CallCandidates(e) => {
                AnyMessageEventContent::CallCandidates(e.content.clone())
            }
            AnySyncMessageEvent::KeyVerificationReady(e) => {
                AnyMessageEventContent::KeyVerificationReady(e.content.clone())
            }
            AnySyncMessageEvent::KeyVerificationStart(e) => {
                AnyMessageEventContent::KeyVerificationStart(e.content.clone())
            }
            AnySyncMessageEvent::KeyVerificationCancel(e) => {
                AnyMessageEventContent::KeyVerificationCancel(e.content.clone())
            }
            AnySyncMessageEvent::KeyVerificationAccept(e) => {
                AnyMessageEventContent::KeyVerificationAccept(e.content.clone())
            }
            AnySyncMessageEvent::KeyVerificationKey(e) => {
                AnyMessageEventContent::KeyVerificationKey(e.content.clone())
            }
            AnySyncMessageEvent::KeyVerificationMac(e) => {
                AnyMessageEventContent::KeyVerificationMac(e.content.clone())
            }
            AnySyncMessageEvent::KeyVerificationDone(e) => {
                AnyMessageEventContent::KeyVerificationDone(e.content.clone())
            }
            AnySyncMessageEvent::Reaction(e) => AnyMessageEventContent::Reaction(e.content.clone()),
            AnySyncMessageEvent::RoomEncrypted(e) => {
                AnyMessageEventContent::RoomEncrypted(e.content.clone())
            }
            AnySyncMessageEvent::RoomMessageFeedback(e) => {
                AnyMessageEventContent::RoomMessageFeedback(e.content.clone())
            }
            AnySyncMessageEvent::RoomRedaction(e) => {
                AnyMessageEventContent::RoomRedaction(e.content.clone())
            }
            AnySyncMessageEvent::Sticker(e) => AnyMessageEventContent::Sticker(e.content.clone()),
            AnySyncMessageEvent::Custom(e) => {
                AnyMessageEventContent::Custom(e.content.clone().into())
            }
        }
    }

    pub fn origin_server_ts(&self) -> &SystemTime {
        match self {
            AnySyncMessageEvent::RoomMessage(e) => &e.origin_server_ts,
            AnySyncMessageEvent::CallAnswer(e) => &e.origin_server_ts,
            AnySyncMessageEvent::CallInvite(e) => &e.origin_server_ts,
            AnySyncMessageEvent::CallHangup(e) => &e.origin_server_ts,
            AnySyncMessageEvent::CallCandidates(e) => &e.origin_server_ts,
            AnySyncMessageEvent::KeyVerificationReady(e) => &e.origin_server_ts,
            AnySyncMessageEvent::KeyVerificationStart(e) => &e.origin_server_ts,
            AnySyncMessageEvent::KeyVerificationCancel(e) => &e.origin_server_ts,
            AnySyncMessageEvent::KeyVerificationAccept(e) => &e.origin_server_ts,
            AnySyncMessageEvent::KeyVerificationKey(e) => &e.origin_server_ts,
            AnySyncMessageEvent::KeyVerificationMac(e) => &e.origin_server_ts,
            AnySyncMessageEvent::KeyVerificationDone(e) => &e.origin_server_ts,
            AnySyncMessageEvent::Reaction(e) => &e.origin_server_ts,
            AnySyncMessageEvent::RoomEncrypted(e) => &e.origin_server_ts,
            AnySyncMessageEvent::RoomMessageFeedback(e) => &e.origin_server_ts,
            AnySyncMessageEvent::RoomRedaction(e) => &e.origin_server_ts,
            AnySyncMessageEvent::Sticker(e) => &e.origin_server_ts,
            AnySyncMessageEvent::Custom(e) => &e.origin_server_ts,
        }
    }

    pub fn unsigned(&self) -> &Unsigned {
        match self {
            AnySyncMessageEvent::RoomMessage(e) => &e.unsigned,
            AnySyncMessageEvent::CallAnswer(e) => &e.unsigned,
            AnySyncMessageEvent::CallInvite(e) => &e.unsigned,
            AnySyncMessageEvent::CallHangup(e) => &e.unsigned,
            AnySyncMessageEvent::CallCandidates(e) => &e.unsigned,
            AnySyncMessageEvent::KeyVerificationReady(e) => &e.unsigned,
            AnySyncMessageEvent::KeyVerificationStart(e) => &e.unsigned,
            AnySyncMessageEvent::KeyVerificationCancel(e) => &e.unsigned,
            AnySyncMessageEvent::KeyVerificationAccept(e) => &e.unsigned,
            AnySyncMessageEvent::KeyVerificationKey(e) => &e.unsigned,
            AnySyncMessageEvent::KeyVerificationMac(e) => &e.unsigned,
            AnySyncMessageEvent::KeyVerificationDone(e) => &e.unsigned,
            AnySyncMessageEvent::Reaction(e) => &e.unsigned,
            AnySyncMessageEvent::RoomEncrypted(e) => &e.unsigned,
            AnySyncMessageEvent::RoomMessageFeedback(e) => &e.unsigned,
            AnySyncMessageEvent::RoomRedaction(e) => &e.unsigned,
            AnySyncMessageEvent::Sticker(e) => &e.unsigned,
            AnySyncMessageEvent::Custom(e) => &e.unsigned,
        }
    }

    pub fn event_id(&self) -> &EventId {
        match self {
            AnySyncMessageEvent::RoomMessage(e) => &e.event_id,
            AnySyncMessageEvent::CallAnswer(e) => &e.event_id,
            AnySyncMessageEvent::CallInvite(e) => &e.event_id,
            AnySyncMessageEvent::CallHangup(e) => &e.event_id,
            AnySyncMessageEvent::CallCandidates(e) => &e.event_id,
            AnySyncMessageEvent::KeyVerificationReady(e) => &e.event_id,
            AnySyncMessageEvent::KeyVerificationStart(e) => &e.event_id,
            AnySyncMessageEvent::KeyVerificationCancel(e) => &e.event_id,
            AnySyncMessageEvent::KeyVerificationAccept(e) => &e.event_id,
            AnySyncMessageEvent::KeyVerificationKey(e) => &e.event_id,
            AnySyncMessageEvent::KeyVerificationMac(e) => &e.event_id,
            AnySyncMessageEvent::KeyVerificationDone(e) => &e.event_id,
            AnySyncMessageEvent::Reaction(e) => &e.event_id,
            AnySyncMessageEvent::RoomEncrypted(e) => &e.event_id,
            AnySyncMessageEvent::RoomMessageFeedback(e) => &e.event_id,
            AnySyncMessageEvent::RoomRedaction(e) => &e.event_id,
            AnySyncMessageEvent::Sticker(e) => &e.event_id,
            AnySyncMessageEvent::Custom(e) => &e.event_id,
        }
    }

    pub fn encryption_info(&self) -> Option<&EncryptionInfo> {
        match self {
            AnySyncMessageEvent::RoomMessage(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::CallAnswer(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::CallInvite(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::CallHangup(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::CallCandidates(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::KeyVerificationReady(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::KeyVerificationStart(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::KeyVerificationCancel(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::KeyVerificationAccept(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::KeyVerificationKey(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::KeyVerificationMac(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::KeyVerificationDone(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::Reaction(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::RoomEncrypted(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::RoomMessageFeedback(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::RoomRedaction(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::Sticker(e) => e.encryption_info.as_ref(),
            AnySyncMessageEvent::Custom(e) => e.encryption_info.as_ref(),
        }
    }
}

impl From<AnySyncMessageEvent> for AnySyncRoomEvent {
    fn from(e: AnySyncMessageEvent) -> Self {
        Self::Message(e)
    }
}

impl From<RumaSyncRoomEvent> for AnySyncRoomEvent {
    fn from(e: RumaSyncRoomEvent) -> Self {
        match e {
            RumaSyncRoomEvent::Message(e) => {
                AnySyncRoomEvent::Message(AnySyncMessageEvent::from_ruma(e, None))
            }
            RumaSyncRoomEvent::State(e) => AnySyncRoomEvent::State(e),
            RumaSyncRoomEvent::RedactedMessage(e) => AnySyncRoomEvent::RedactedMessage(e),
            RumaSyncRoomEvent::RedactedState(e) => AnySyncRoomEvent::RedactedState(e),
        }
    }
}
