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
            redaction::{RedactionEventContent, SyncRedactionEvent},
        },
        sticker::StickerEventContent,
        AnyRedactedSyncMessageEvent, AnyRedactedSyncStateEvent,
        AnySyncMessageEvent as RumaAnySyncMessageEvent, AnySyncRoomEvent as RumaSyncRoomEvent,
        AnySyncStateEvent, MessageEventContent as MessageEventContentTrait,
        SyncMessageEvent as RumaSyncMessageEvent, Unsigned,
    },
    identifiers::{DeviceIdBox, EventId, UserId},
};
use ruma::events::{EventContent, RoomEventContent};
use serde_json::{value::RawValue as RawJsonValue, Value as JsonValue};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum VerificationState {
    Trusted,
    Untrusted,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncryptionInfo {
    sender: UserId,
    sender_device: DeviceIdBox,
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

impl SyncMessageEvent<RedactionEventContent> {
    pub fn from_ruma_redaction(
        event: SyncRedactionEvent,
        encryption_info: Option<EncryptionInfo>,
    ) -> SyncMessageEvent<RedactionEventContent> {
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
    RoomRedaction(SyncMessageEvent<RedactionEventContent>),
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
fromEvent!(RoomRedaction, RedactionEventContent);
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
                SyncMessageEvent::from_ruma_redaction(e, info).into()
            }
            RumaAnySyncMessageEvent::Sticker(e) => SyncMessageEvent::from_ruma(e, info).into(),
            RumaAnySyncMessageEvent::Custom(e) => {
                let event: SyncMessageEvent<CustomEventContent> = e.into();
                AnySyncMessageEvent::Custom(event)
            }
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
