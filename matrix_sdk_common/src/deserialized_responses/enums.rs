use std::{convert::TryInto, sync::Arc, time::SystemTime};

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
            redaction::SyncRedactionEvent as RumaSyncRedactionEvent,
        },
        sticker::StickerEventContent,
        AnyRedactedSyncMessageEvent, AnyRedactedSyncStateEvent,
        AnySyncMessageEvent as RumaAnySyncMessageEvent, AnySyncRoomEvent as RumaSyncRoomEvent,
        AnySyncStateEvent, MessageEventContent as MessageEventContentT,
        SyncMessageEvent as RumaSyncMessageEvent, Unsigned,
    },
    identifiers::{EventId, UserId},
};
use ruma::{
    events::{
        room::redaction::RedactionEventContent,
        AnyMessageEventContent as RumaAnyMessageEventContent,
    },
    serde::Raw,
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::events::{
    CustomEventContent, EncryptionInfo, InvalidEventContent, SyncMessageEvent, SyncRedactionEvent,
};

/// Any sync room event (room event without a `room_id`, as returned in `/sync` responses)
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
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

/// Enum holding the different event types a sync timeline can hold.
#[derive(Clone, Debug)]
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
    Invalid(SyncMessageEvent<InvalidEventContent>),
}

fn from_json<T, E>(json: Value) -> Result<T, E>
where
    T: serde::de::DeserializeOwned,
    E: serde::de::Error,
{
    serde_json::from_value(json).map_err(serde::de::Error::custom)
}

fn to_json<T, E>(object: T) -> Result<Value, E>
where
    T: Serialize,
    E: serde::ser::Error,
{
    serde_json::to_value(object).map_err(serde::ser::Error::custom)
}

impl<'de> Deserialize<'de> for AnySyncMessageEvent {
    fn deserialize<D>(deseriazlier: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Clone, Debug, Deserialize)]
        struct UntypedEvent {
            pub content: Value,
            #[serde(rename = "type")]
            pub event_type: String,
            pub event_id: EventId,
            pub sender: UserId,
            #[serde(with = "ruma::serde::time::ms_since_unix_epoch")]
            pub origin_server_ts: SystemTime,
            pub unsigned: Unsigned,
            pub encryption_info: Option<EncryptionInfo>,
        };

        let json = <Value>::deserialize(deseriazlier)?;
        let encryption_info: Option<EncryptionInfo> = json
            .get("encryption_info")
            .cloned()
            .map(from_json)
            .transpose()?;

        let event: Raw<RumaAnySyncMessageEvent> = from_json(json)?;

        let event = match event.deserialize() {
            Ok(e) => AnySyncMessageEvent::from_ruma(e, encryption_info),
            Err(e) => {
                // The event is invalid, yet it might just be that the content
                // is invalid, try to deserialize the main event fields leaving
                // the content untyped.
                let event: UntypedEvent =
                    serde_json::from_str(event.json().get()).map_err(serde::de::Error::custom)?;

                let content = InvalidEventContent {
                    deserialization_error: Arc::new(Some(e)),
                    event_type: event.event_type,
                    json: event.content,
                };

                let event = SyncMessageEvent {
                    content,
                    event_id: event.event_id,
                    sender: event.sender,
                    origin_server_ts: event.origin_server_ts,
                    unsigned: event.unsigned,
                    encryption_info,
                };

                AnySyncMessageEvent::from(event)
            }
        };

        Ok(event)
    }
}

impl Serialize for AnySyncMessageEvent {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Clone, Debug, Serialize)]
        struct UntypedEvent<'a> {
            pub content: &'a Value,
            #[serde(rename = "type")]
            pub event_type: &'a str,
            pub event_id: &'a EventId,
            pub sender: &'a UserId,
            #[serde(with = "ruma::serde::time::ms_since_unix_epoch")]
            pub origin_server_ts: &'a SystemTime,
            pub unsigned: &'a Unsigned,
        };

        let encryption_info = self.encryption_info().map(to_json).transpose()?;

        let mut event = if let AnySyncMessageEvent::Invalid(e) = self {
            let event = UntypedEvent {
                content: &e.content.json,
                event_id: &e.event_id,
                event_type: &e.content.event_type,
                origin_server_ts: &e.origin_server_ts,
                sender: &e.sender,
                unsigned: &e.unsigned,
            };

            to_json(event)?
        } else {
            // TODO get rid of this clone.
            let event: RumaAnySyncMessageEvent = self.clone().try_into().unwrap();
            to_json(event)?
        };

        if let Some(info) = encryption_info {
            event
                .as_object_mut()
                .expect("Didn't serialize event into an object")
                .insert("encryption_info".to_string(), info);
        }

        event.serialize(serializer)
    }
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
fromEvent!(Invalid, InvalidEventContent);

impl From<SyncRedactionEvent> for AnySyncMessageEvent {
    fn from(e: SyncRedactionEvent) -> Self {
        Self::RoomRedaction(e)
    }
}

impl<C: MessageEventContentT> Into<RumaSyncMessageEvent<C>> for SyncMessageEvent<C> {
    fn into(self) -> RumaSyncMessageEvent<C> {
        RumaSyncMessageEvent {
            content: self.content,
            event_id: self.event_id,
            sender: self.sender,
            origin_server_ts: self.origin_server_ts,
            unsigned: self.unsigned,
        }
    }
}

impl TryInto<RumaAnySyncMessageEvent> for AnySyncMessageEvent {
    type Error = &'static str;

    fn try_into(self) -> Result<RumaAnySyncMessageEvent, Self::Error> {
        Ok(match self {
            AnySyncMessageEvent::RoomMessage(e) => RumaAnySyncMessageEvent::RoomMessage(e.into()),
            AnySyncMessageEvent::CallAnswer(e) => RumaAnySyncMessageEvent::CallAnswer(e.into()),
            AnySyncMessageEvent::CallInvite(e) => RumaAnySyncMessageEvent::CallInvite(e.into()),
            AnySyncMessageEvent::CallHangup(e) => RumaAnySyncMessageEvent::CallHangup(e.into()),
            AnySyncMessageEvent::CallCandidates(e) => {
                RumaAnySyncMessageEvent::CallCandidates(e.into())
            }
            AnySyncMessageEvent::KeyVerificationReady(e) => {
                RumaAnySyncMessageEvent::KeyVerificationReady(e.into())
            }
            AnySyncMessageEvent::KeyVerificationStart(e) => {
                RumaAnySyncMessageEvent::KeyVerificationStart(e.into())
            }
            AnySyncMessageEvent::KeyVerificationCancel(e) => {
                RumaAnySyncMessageEvent::KeyVerificationCancel(e.into())
            }
            AnySyncMessageEvent::KeyVerificationAccept(e) => {
                RumaAnySyncMessageEvent::KeyVerificationAccept(e.into())
            }
            AnySyncMessageEvent::KeyVerificationKey(e) => {
                RumaAnySyncMessageEvent::KeyVerificationKey(e.into())
            }
            AnySyncMessageEvent::KeyVerificationMac(e) => {
                RumaAnySyncMessageEvent::KeyVerificationMac(e.into())
            }
            AnySyncMessageEvent::KeyVerificationDone(e) => {
                RumaAnySyncMessageEvent::KeyVerificationDone(e.into())
            }
            AnySyncMessageEvent::Reaction(e) => RumaAnySyncMessageEvent::Reaction(e.into()),
            AnySyncMessageEvent::RoomEncrypted(e) => {
                RumaAnySyncMessageEvent::RoomEncrypted(e.into())
            }
            AnySyncMessageEvent::RoomMessageFeedback(e) => {
                RumaAnySyncMessageEvent::RoomMessageFeedback(e.into())
            }
            AnySyncMessageEvent::RoomRedaction(e) => {
                RumaAnySyncMessageEvent::RoomRedaction(RumaSyncRedactionEvent {
                    content: e.content,
                    event_id: e.event_id,
                    sender: e.sender,
                    origin_server_ts: e.origin_server_ts,
                    unsigned: e.unsigned,
                    redacts: e.redacts,
                })
            }
            AnySyncMessageEvent::Sticker(e) => RumaAnySyncMessageEvent::Sticker(e.into()),
            AnySyncMessageEvent::Custom(e) => {
                RumaAnySyncMessageEvent::Custom(RumaSyncMessageEvent {
                    content: RumaCustomEventContent {
                        event_type: e.content.event_type,
                        json: e.content.json,
                    },
                    event_id: e.event_id,
                    sender: e.sender,
                    origin_server_ts: e.origin_server_ts,
                    unsigned: e.unsigned,
                })
            }
            AnySyncMessageEvent::Invalid(_) => return Err("Unsupported Ruma event: invalid event"),
        })
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
            AnySyncMessageEvent::Invalid(e) => &e.sender,
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
            AnySyncMessageEvent::Custom(e) => AnyMessageEventContent::Custom(e.content.clone()),
            AnySyncMessageEvent::Invalid(e) => AnyMessageEventContent::Invalid(e.content.clone()),
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
            AnySyncMessageEvent::Invalid(e) => &e.origin_server_ts,
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
            AnySyncMessageEvent::Invalid(e) => &e.unsigned,
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
            AnySyncMessageEvent::Invalid(e) => &e.event_id,
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
            AnySyncMessageEvent::Invalid(e) => e.encryption_info.as_ref(),
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

/// Enum over the different contents a Matrix event can contain.
///
/// This differs from the Ruma provided one in that it can contain invalid
/// content variant.
///
/// `TryInto` and `From` are implemented for this enum to ease the conversion
/// from the Ruma variant.
#[derive(Clone, Debug)]
pub enum AnyMessageEventContent {
    RoomMessage(MessageEventContent),
    CallAnswer(AnswerEventContent),
    CallInvite(InviteEventContent),
    CallHangup(HangupEventContent),
    CallCandidates(CandidatesEventContent),
    KeyVerificationReady(ReadyEventContent),
    KeyVerificationStart(StartEventContent),
    KeyVerificationCancel(CancelEventContent),
    KeyVerificationAccept(AcceptEventContent),
    KeyVerificationKey(KeyEventContent),
    KeyVerificationMac(MacEventContent),
    KeyVerificationDone(DoneEventContent),
    Reaction(ReactionEventContent),
    RoomEncrypted(EncryptedEventContent),
    RoomMessageFeedback(FeedbackEventContent),
    RoomRedaction(RedactionEventContent),
    Sticker(StickerEventContent),
    Custom(CustomEventContent),
    Invalid(InvalidEventContent),
}

impl From<RumaAnyMessageEventContent> for AnyMessageEventContent {
    fn from(content: RumaAnyMessageEventContent) -> Self {
        match content {
            RumaAnyMessageEventContent::CallAnswer(e) => Self::CallAnswer(e),
            RumaAnyMessageEventContent::CallInvite(e) => Self::CallInvite(e),
            RumaAnyMessageEventContent::CallHangup(e) => Self::CallHangup(e),
            RumaAnyMessageEventContent::CallCandidates(e) => Self::CallCandidates(e),
            RumaAnyMessageEventContent::KeyVerificationReady(e) => Self::KeyVerificationReady(e),
            RumaAnyMessageEventContent::KeyVerificationStart(e) => Self::KeyVerificationStart(e),
            RumaAnyMessageEventContent::KeyVerificationCancel(e) => Self::KeyVerificationCancel(e),
            RumaAnyMessageEventContent::KeyVerificationAccept(e) => Self::KeyVerificationAccept(e),
            RumaAnyMessageEventContent::KeyVerificationKey(e) => Self::KeyVerificationKey(e),
            RumaAnyMessageEventContent::KeyVerificationMac(e) => Self::KeyVerificationMac(e),
            RumaAnyMessageEventContent::KeyVerificationDone(e) => Self::KeyVerificationDone(e),
            RumaAnyMessageEventContent::Reaction(e) => Self::Reaction(e),
            RumaAnyMessageEventContent::RoomEncrypted(e) => Self::RoomEncrypted(e),
            RumaAnyMessageEventContent::RoomMessage(e) => Self::RoomMessage(e),
            RumaAnyMessageEventContent::RoomMessageFeedback(e) => Self::RoomMessageFeedback(e),
            RumaAnyMessageEventContent::RoomRedaction(e) => Self::RoomRedaction(e),
            RumaAnyMessageEventContent::Sticker(e) => Self::Sticker(e),
            RumaAnyMessageEventContent::Custom(e) => Self::Custom(e.into()),
        }
    }
}

impl TryInto<RumaAnyMessageEventContent> for AnyMessageEventContent {
    type Error = &'static str;

    fn try_into(self) -> Result<RumaAnyMessageEventContent, Self::Error> {
        Ok(match self {
            Self::RoomMessage(e) => RumaAnyMessageEventContent::RoomMessage(e),
            Self::CallAnswer(e) => RumaAnyMessageEventContent::CallAnswer(e),
            Self::CallInvite(e) => RumaAnyMessageEventContent::CallInvite(e),
            Self::CallHangup(e) => RumaAnyMessageEventContent::CallHangup(e),
            Self::CallCandidates(e) => RumaAnyMessageEventContent::CallCandidates(e),
            Self::KeyVerificationReady(e) => RumaAnyMessageEventContent::KeyVerificationReady(e),
            Self::KeyVerificationStart(e) => RumaAnyMessageEventContent::KeyVerificationStart(e),
            Self::KeyVerificationCancel(e) => RumaAnyMessageEventContent::KeyVerificationCancel(e),
            Self::KeyVerificationAccept(e) => RumaAnyMessageEventContent::KeyVerificationAccept(e),
            Self::KeyVerificationKey(e) => RumaAnyMessageEventContent::KeyVerificationKey(e),
            Self::KeyVerificationMac(e) => RumaAnyMessageEventContent::KeyVerificationMac(e),
            Self::KeyVerificationDone(e) => RumaAnyMessageEventContent::KeyVerificationDone(e),
            Self::Reaction(e) => RumaAnyMessageEventContent::Reaction(e),
            Self::RoomEncrypted(e) => RumaAnyMessageEventContent::RoomEncrypted(e),
            Self::RoomMessageFeedback(e) => RumaAnyMessageEventContent::RoomMessageFeedback(e),
            Self::RoomRedaction(e) => RumaAnyMessageEventContent::RoomRedaction(e),
            Self::Sticker(e) => RumaAnyMessageEventContent::Sticker(e),
            Self::Custom(e) => RumaAnyMessageEventContent::Custom(e.into()),
            Self::Invalid(_) => {
                return Err("Invalid events cannot be converted into the Ruma variant of this enum")
            }
        })
    }
}

#[cfg(test)]
mod test {
    use super::AnySyncMessageEvent;
    use serde_json::json;

    #[test]
    fn serialize_deserialize_message() {
        let event = json!({
            "content": {
                "body": "Hello world",
                "msgtype": "m.text"
            },
            "event_id": "$iQvG0pdxqYpsXkm6KhCC7J6DomnPgqtszLOVKbyeN48",
            "origin_server_ts": 1613726073560i64,
            "sender": "@example:localhost",
            "type": "m.room.message",
            "unsigned": {
                "age": 59,
                "transaction_id": "4c5d3403-d4ae-4e23-af21-ebcc2cb7b093"
            }
        });

        let deserialized: AnySyncMessageEvent = serde_json::from_value(event.clone()).unwrap();

        if let AnySyncMessageEvent::RoomMessage(_) = &deserialized {
        } else {
            panic!("Invalid event type");
        }

        let serialized = serde_json::to_value(deserialized).unwrap();

        assert_eq!(event, serialized);
    }

    #[test]
    fn serialize_deserialize_invalid_content() {
        let event = json!({
            "content": {
                "msgtype": "m.text"
            },
            "event_id": "$iQvG0pdxqYpsXkm6KhCC7J6DomnPgqtszLOVKbyeN48",
            "origin_server_ts": 1613726073560i64,
            "sender": "@example:localhost",
            "type": "m.room.message",
            "unsigned": {
                "age": 59,
                "transaction_id": "4c5d3403-d4ae-4e23-af21-ebcc2cb7b093"
            }
        });

        let deserialized: AnySyncMessageEvent = serde_json::from_value(event.clone()).unwrap();

        if let AnySyncMessageEvent::Invalid(_) = &deserialized {
        } else {
            panic!("Invalid event type");
        }

        let serialized = serde_json::to_value(deserialized).unwrap();

        assert_eq!(event, serialized);
    }
}
