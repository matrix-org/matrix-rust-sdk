use std::time::SystemTime;

use crate::{
    events::{
        call::{
            answer::AnswerEventContent, candidates::CandidatesEventContent,
            hangup::HangupEventContent, invite::InviteEventContent,
        },
        key::verification::{
            accept::AcceptEventContent, cancel::CancelEventContent, done::DoneEventContent,
            key::KeyEventContent, mac::MacEventContent, ready::ReadyEventContent,
            start::StartEventContent,
        },
        reaction::ReactionEventContent,
        room::{
            encrypted::EncryptedEventContent,
            message::{feedback::FeedbackEventContent, MessageEventContent},
        },
        sticker::StickerEventContent,
        AnyRedactedSyncMessageEvent, AnyRedactedSyncStateEvent,
        AnySyncMessageEvent as RumaAnySyncMessageEvent, AnySyncRoomEvent as RumaSyncRoomEvent,
        AnySyncStateEvent, Unsigned,
    },
    identifiers::{EventId, UserId},
};
use ruma::events::AnyMessageEventContent;
use serde::{Deserialize, Serialize};

use super::events::{
    CustomEventContent, EncryptionInfo, InvalidEventContent, SyncMessageEvent, SyncRedactionEvent,
};

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
    Invalid(SyncMessageEvent<InvalidEventContent>),
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
            AnySyncMessageEvent::Custom(e) => {
                AnyMessageEventContent::Custom(e.content.clone().into())
            }
            AnySyncMessageEvent::Invalid(e) => {
                todo!()
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
