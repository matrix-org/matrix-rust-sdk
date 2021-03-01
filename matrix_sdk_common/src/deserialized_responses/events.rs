use std::{collections::BTreeMap, sync::Arc, time::SystemTime};

use crate::{
    events::{
        custom::CustomEventContent as RumaCustomEventContent,
        room::redaction::{RedactionEventContent, SyncRedactionEvent as RumaSyncRedationEvent},
        MessageEventContent as MessageEventContentTrait, SyncMessageEvent as RumaSyncMessageEvent,
        Unsigned,
    },
    identifiers::{DeviceIdBox, EventId, UserId},
};

use ruma::{
    events::{EventContent, RoomEventContent},
    DeviceKeyAlgorithm,
};
use serde_json::{value::RawValue as RawJsonValue, Value as JsonValue};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum VerificationState {
    Trusted,
    Untrusted,
    UnknownDevice,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum AlgorithmInfo {
    MegolmV1AesSha2 {
        curve25519_key: String,
        sender_claimed_keys: BTreeMap<DeviceKeyAlgorithm, String>,
        forwarding_curve25519_key_chain: Vec<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncryptionInfo {
    pub sender: UserId,
    pub sender_device: DeviceIdBox,
    pub algorithm_info: AlgorithmInfo,
    pub verification_state: VerificationState,
}

/// An invalid event content containing the deserialization error and the raw
/// JSON
#[derive(Clone, Debug, Serialize)]
pub struct InvalidEventContent {
    /// The event type string.
    #[serde(skip)]
    pub deserialization_error: Arc<Option<serde_json::Error>>,

    /// The underlying type of the event.
    #[serde(skip)]
    pub event_type: String,

    /// The actual raw `content` JSON object.
    #[serde(flatten)]
    pub data: JsonValue,
}

impl EventContent for InvalidEventContent {
    fn event_type(&self) -> &str {
        &self.event_type
    }

    fn from_parts(_: &str, _: Box<RawJsonValue>) -> Result<Self, serde_json::Error> {
        unreachable!()
    }
}

impl RoomEventContent for InvalidEventContent {}
impl MessageEventContentTrait for InvalidEventContent {}

/// A custom event's type and `content` JSON object.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CustomEventContent {
    /// The event type string.
    #[serde(skip)]
    pub event_type: String,

    /// The actual `content` JSON object.
    #[serde(flatten)]
    pub data: BTreeMap<String, JsonValue>,
}

impl From<RumaCustomEventContent> for CustomEventContent {
    fn from(e: RumaCustomEventContent) -> Self {
        Self {
            event_type: e.event_type,
            data: e.data,
        }
    }
}

impl EventContent for CustomEventContent {
    fn event_type(&self) -> &str {
        &self.event_type
    }

    fn from_parts(event_type: &str, content: Box<RawJsonValue>) -> Result<Self, serde_json::Error> {
        let data = serde_json::from_str(content.get())?;
        Ok(Self {
            event_type: event_type.to_string(),
            data,
        })
    }
}

impl Into<RumaCustomEventContent> for CustomEventContent {
    fn into(self) -> RumaCustomEventContent {
        RumaCustomEventContent {
            event_type: self.event_type,
            data: self.data,
        }
    }
}

impl RoomEventContent for CustomEventContent {}
impl MessageEventContentTrait for CustomEventContent {}

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
    #[serde(with = "ruma::serde::time::ms_since_unix_epoch")]
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
    #[serde(with = "ruma::serde::time::ms_since_unix_epoch")]
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
