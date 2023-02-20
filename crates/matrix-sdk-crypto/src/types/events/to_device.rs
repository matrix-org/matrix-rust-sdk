// Copyright 2022 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::BTreeMap, fmt::Debug};

use ruma::{
    events::{
        key::verification::{
            accept::ToDeviceKeyVerificationAcceptEvent, cancel::ToDeviceKeyVerificationCancelEvent,
            done::ToDeviceKeyVerificationDoneEvent, key::ToDeviceKeyVerificationKeyEvent,
            mac::ToDeviceKeyVerificationMacEvent, ready::ToDeviceKeyVerificationReadyEvent,
            request::ToDeviceKeyVerificationRequestEvent, start::ToDeviceKeyVerificationStartEvent,
        },
        secret::request::{SecretName, ToDeviceSecretRequestEvent},
        EventContent, ToDeviceEventType,
    },
    serde::Raw,
    OwnedUserId, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{
    value::{to_raw_value, RawValue},
    Value,
};
use zeroize::Zeroize;

use super::{
    dummy::DummyEvent,
    forwarded_room_key::{ForwardedRoomKeyContent, ForwardedRoomKeyEvent},
    room::encrypted::EncryptedToDeviceEvent,
    room_key::RoomKeyEvent,
    room_key_request::RoomKeyRequestEvent,
    room_key_withheld::RoomKeyWithheldEvent,
    secret_send::SecretSendEvent,
    EventType,
};
use crate::types::events::from_str;

/// An enum over the various to-device events we support.
#[derive(Debug)]
pub enum ToDeviceEvents {
    /// A to-device event of an unknown or custom type.
    Custom(ToDeviceCustomEvent),
    /// The `m.dummy` to-device event.
    Dummy(DummyEvent),

    /// The `m.key.verification.accept` to-device event.
    KeyVerificationAccept(ToDeviceKeyVerificationAcceptEvent),
    /// The `m.key.verification.cancel` to-device event.
    KeyVerificationCancel(ToDeviceKeyVerificationCancelEvent),
    /// The `m.key.verification.key` to-device event.
    KeyVerificationKey(ToDeviceKeyVerificationKeyEvent),
    /// The `m.key.verification.mac` to-device event.
    KeyVerificationMac(ToDeviceKeyVerificationMacEvent),
    /// The `m.key.verification.done` to-device event.
    KeyVerificationDone(ToDeviceKeyVerificationDoneEvent),
    /// The `m.key.verification.start` to-device event.
    KeyVerificationStart(ToDeviceKeyVerificationStartEvent),
    /// The `m.key.verification.ready` to-device event.
    KeyVerificationReady(ToDeviceKeyVerificationReadyEvent),
    /// The `m.key.verification.request` to-device event.
    KeyVerificationRequest(ToDeviceKeyVerificationRequestEvent),

    /// The `m.room.encrypted` to-device event.
    RoomEncrypted(EncryptedToDeviceEvent),
    /// The `m.room_key` to-device event.
    RoomKey(RoomKeyEvent),
    /// The `m.room_key_request` to-device event.
    RoomKeyRequest(RoomKeyRequestEvent),
    /// The `m.forwarded_room_key` to-device event.
    ForwardedRoomKey(Box<ForwardedRoomKeyEvent>),
    /// The `m.secret.send` to-device event.
    SecretSend(SecretSendEvent),
    /// The `m.secret.request` to-device event.
    SecretRequest(ToDeviceSecretRequestEvent),
    /// The `m.room_key.withheld`  to-device event.
    RoomKeyWithheld(RoomKeyWithheldEvent),
}

impl ToDeviceEvents {
    /// The sender of the to-device event.
    pub fn sender(&self) -> &UserId {
        match self {
            ToDeviceEvents::Custom(e) => &e.sender,
            ToDeviceEvents::Dummy(e) => &e.sender,

            ToDeviceEvents::KeyVerificationAccept(e) => &e.sender,
            ToDeviceEvents::KeyVerificationCancel(e) => &e.sender,
            ToDeviceEvents::KeyVerificationKey(e) => &e.sender,
            ToDeviceEvents::KeyVerificationMac(e) => &e.sender,
            ToDeviceEvents::KeyVerificationDone(e) => &e.sender,
            ToDeviceEvents::KeyVerificationStart(e) => &e.sender,
            ToDeviceEvents::KeyVerificationReady(e) => &e.sender,
            ToDeviceEvents::KeyVerificationRequest(e) => &e.sender,

            ToDeviceEvents::RoomEncrypted(e) => &e.sender,
            ToDeviceEvents::RoomKey(e) => &e.sender,
            ToDeviceEvents::RoomKeyRequest(e) => &e.sender,
            ToDeviceEvents::ForwardedRoomKey(e) => &e.sender,

            ToDeviceEvents::SecretSend(e) => &e.sender,
            ToDeviceEvents::SecretRequest(e) => &e.sender,
            ToDeviceEvents::RoomKeyWithheld(e) => &e.sender,
        }
    }

    /// The event type of the to-device event.
    pub fn event_type(&self) -> ToDeviceEventType {
        match self {
            ToDeviceEvents::Custom(e) => ToDeviceEventType::from(e.event_type.to_owned()),
            ToDeviceEvents::Dummy(_) => ToDeviceEventType::Dummy,

            ToDeviceEvents::KeyVerificationAccept(e) => e.content.event_type(),
            ToDeviceEvents::KeyVerificationCancel(e) => e.content.event_type(),
            ToDeviceEvents::KeyVerificationKey(e) => e.content.event_type(),
            ToDeviceEvents::KeyVerificationMac(e) => e.content.event_type(),
            ToDeviceEvents::KeyVerificationDone(e) => e.content.event_type(),
            ToDeviceEvents::KeyVerificationStart(e) => e.content.event_type(),
            ToDeviceEvents::KeyVerificationReady(e) => e.content.event_type(),
            ToDeviceEvents::KeyVerificationRequest(e) => e.content.event_type(),

            ToDeviceEvents::RoomEncrypted(_) => ToDeviceEventType::RoomEncrypted,
            ToDeviceEvents::RoomKey(_) => ToDeviceEventType::RoomKey,
            ToDeviceEvents::RoomKeyRequest(_) => ToDeviceEventType::RoomKeyRequest,
            ToDeviceEvents::ForwardedRoomKey(_) => ToDeviceEventType::ForwardedRoomKey,

            ToDeviceEvents::SecretSend(_) => ToDeviceEventType::SecretSend,
            ToDeviceEvents::SecretRequest(e) => e.content.event_type(),
            // Todo add withheld type to ruma
            ToDeviceEvents::RoomKeyWithheld(e) => {
                ToDeviceEventType::from(e.content.event_type().to_owned())
            }
        }
    }

    /// Serialize this event into a Raw variant while zeroizing any secrets it
    /// might contain.
    ///
    /// Secrets in Matrix are usually base64 encoded strings, zeroizing in this
    /// context means that the secret will be converted into an empty string.
    ///
    /// The following secrets will be zeroized by this method:
    ///
    /// * `m.room_key` - The `session_key` field.
    /// * `m.forwarded_room_key` - The `session_key` field.
    /// * `m.secret.send` - The `secret` field will be zeroized, unless the
    /// secret name of the matching `m.secret.request` event was
    /// `m.megolm_backup.v1`.
    ///
    /// **Warning**: Some events won't be able to be deserialized into the
    /// `ToDeviceEvents` type again since they might expect a valid `SessionKey`
    /// for `m.room.key` events or valid base64 for some other secrets.
    ///
    /// You can do a couple of things to avoid this problem:
    ///
    /// 1. Call `Raw::cast()` to convert the event to another, less strict type.
    /// [`AnyToDeviceEvent`] from Ruma will work.
    ///
    /// 2. Call `Raw::deserialize_as()` to deserialize into a less strict type.
    ///
    /// 3. Pass the event over FFI, losing the exact type information, this will
    /// mostl likely end up using a less strict type naturally.
    ///
    /// [`AnyToDeviceEvent`]: ruma::events::AnyToDeviceEvent
    pub(crate) fn serialize_zeroized(self) -> Result<Raw<ToDeviceEvents>, serde_json::Error> {
        let serialized = match self {
            ToDeviceEvents::Custom(_)
            | ToDeviceEvents::Dummy(_)
            | ToDeviceEvents::KeyVerificationAccept(_)
            | ToDeviceEvents::KeyVerificationCancel(_)
            | ToDeviceEvents::KeyVerificationKey(_)
            | ToDeviceEvents::KeyVerificationMac(_)
            | ToDeviceEvents::KeyVerificationDone(_)
            | ToDeviceEvents::KeyVerificationStart(_)
            | ToDeviceEvents::KeyVerificationReady(_)
            | ToDeviceEvents::KeyVerificationRequest(_)
            | ToDeviceEvents::RoomEncrypted(_)
            | ToDeviceEvents::RoomKeyRequest(_)
            | ToDeviceEvents::RoomKeyWithheld(_)
            | ToDeviceEvents::SecretRequest(_) => Raw::from_json(to_raw_value(&self)?),
            ToDeviceEvents::RoomKey(e) => {
                let event_type = e.content.event_type();
                let content = e.content.serialize_zeroized()?;

                #[derive(Serialize)]
                struct Helper<'a, C> {
                    sender: &'a UserId,
                    content: &'a Raw<C>,
                    #[serde(rename = "type")]
                    event_type: &'a str,
                }

                let helper = Helper { sender: &e.sender, content: &content, event_type };

                let raw_value = to_raw_value(&helper)?;

                Raw::from_json(raw_value)
            }
            ToDeviceEvents::ForwardedRoomKey(mut e) => {
                match &mut e.content {
                    ForwardedRoomKeyContent::MegolmV1AesSha2(c) => c.session_key.zeroize(),
                    #[cfg(feature = "experimental-algorithms")]
                    ForwardedRoomKeyContent::MegolmV2AesSha2(c) => c.session_key.zeroize(),
                    ForwardedRoomKeyContent::Unknown(_) => (),
                }

                Raw::from_json(to_raw_value(&e)?)
            }
            ToDeviceEvents::SecretSend(mut e) => {
                if let Some(SecretName::RecoveryKey) = e.content.secret_name {
                    // We don't zeroize the recovery key since it requires
                    // additional requests and possibly user-interaction to be
                    // verified. We let the user deal with this.
                } else {
                    e.content.secret.zeroize();
                }
                Raw::from_json(to_raw_value(&e)?)
            }
        };

        Ok(serialized)
    }
}

/// A to-device event with an unknown type and content.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ToDeviceCustomEvent {
    /// The sender of the to-device event.
    pub sender: OwnedUserId,
    /// The content of the to-device event.
    pub content: BTreeMap<String, Value>,
    /// The type of the to-device event.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Any other unknown data of the to-device event.
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

/// Generic to-device event with a known type and content.
#[derive(Debug)]
pub struct ToDeviceEvent<C>
where
    C: EventType + Debug + Sized + Serialize,
{
    /// The sender of the to-device event.
    pub sender: OwnedUserId,
    /// The content of the to-device event.
    pub content: C,
    /// Any other unknown data of the to-device event.
    pub(crate) other: BTreeMap<String, Value>,
}

impl<C: EventType + Debug + Sized + Serialize> ToDeviceEvent<C> {
    /// Create a new `ToDeviceEvent`.
    pub fn new(sender: OwnedUserId, content: C) -> ToDeviceEvent<C> {
        ToDeviceEvent { sender, content, other: Default::default() }
    }
}

impl<C> Serialize for ToDeviceEvent<C>
where
    C: EventType + Debug + Sized + Serialize,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(Serialize)]
        struct Helper<'a, C> {
            sender: &'a UserId,
            content: &'a C,
            #[serde(rename = "type")]
            event_type: &'a str,
            #[serde(flatten)]
            other: &'a BTreeMap<String, Value>,
        }

        let event_type = self.content.event_type();

        let helper =
            Helper { sender: &self.sender, content: &self.content, event_type, other: &self.other };

        helper.serialize(serializer)
    }
}

impl<'de, C> Deserialize<'de> for ToDeviceEvent<C>
where
    C: EventType + Debug + Sized + Deserialize<'de> + Serialize,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Helper<C> {
            sender: OwnedUserId,
            content: C,
            // We're deserializing the `type` field here so it doesn't end up in `other`.
            #[allow(dead_code)]
            #[serde(rename = "type")]
            event_type: String,
            #[serde(flatten)]
            other: BTreeMap<String, Value>,
        }

        let helper: Helper<C> = Helper::deserialize(deserializer)?;

        if helper.event_type == helper.content.event_type() {
            Ok(Self { sender: helper.sender, content: helper.content, other: helper.other })
        } else {
            Err(serde::de::Error::custom(format!(
                "Mismatched event type, got {}, expected {}",
                helper.event_type,
                helper.content.event_type()
            )))
        }
    }
}

impl<'de> Deserialize<'de> for ToDeviceEvents {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Debug, Deserialize)]
        struct Helper<'a> {
            #[serde(rename = "type")]
            event_type: &'a str,
        }

        let json = Box::<RawValue>::deserialize(deserializer)?;
        let helper: Helper<'_> =
            serde_json::from_str(json.get()).map_err(serde::de::Error::custom)?;

        let json = json.get();

        Ok(match helper.event_type {
            "m.dummy" => ToDeviceEvents::Dummy(from_str(json)?),

            "m.key.verification.accept" => ToDeviceEvents::KeyVerificationAccept(from_str(json)?),
            "m.key.verification.cancel" => ToDeviceEvents::KeyVerificationCancel(from_str(json)?),
            "m.key.verification.done" => ToDeviceEvents::KeyVerificationDone(from_str(json)?),
            "m.key.verification.key" => ToDeviceEvents::KeyVerificationKey(from_str(json)?),
            "m.key.verification.mac" => ToDeviceEvents::KeyVerificationMac(from_str(json)?),
            "m.key.verification.start" => ToDeviceEvents::KeyVerificationStart(from_str(json)?),
            "m.key.verification.ready" => ToDeviceEvents::KeyVerificationReady(from_str(json)?),
            "m.key.verification.request" => ToDeviceEvents::KeyVerificationRequest(from_str(json)?),

            "m.room.encrypted" => ToDeviceEvents::RoomEncrypted(from_str(json)?),
            "m.room_key" => ToDeviceEvents::RoomKey(from_str(json)?),
            "m.forwarded_room_key" => ToDeviceEvents::ForwardedRoomKey(from_str(json)?),
            "m.room_key_request" => ToDeviceEvents::RoomKeyRequest(from_str(json)?),
            "m.room_key.withheld" => ToDeviceEvents::RoomKeyWithheld(from_str(json)?),

            "m.secret.send" => ToDeviceEvents::SecretSend(from_str(json)?),
            "m.secret.request" => ToDeviceEvents::SecretRequest(from_str(json)?),

            _ => ToDeviceEvents::Custom(from_str(json)?),
        })
    }
}

impl Serialize for ToDeviceEvents {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            ToDeviceEvents::Custom(e) => e.serialize(serializer),
            ToDeviceEvents::Dummy(e) => e.serialize(serializer),

            ToDeviceEvents::KeyVerificationAccept(e) => e.serialize(serializer),
            ToDeviceEvents::KeyVerificationCancel(e) => e.serialize(serializer),
            ToDeviceEvents::KeyVerificationKey(e) => e.serialize(serializer),
            ToDeviceEvents::KeyVerificationMac(e) => e.serialize(serializer),
            ToDeviceEvents::KeyVerificationDone(e) => e.serialize(serializer),
            ToDeviceEvents::KeyVerificationStart(e) => e.serialize(serializer),
            ToDeviceEvents::KeyVerificationReady(e) => e.serialize(serializer),
            ToDeviceEvents::KeyVerificationRequest(e) => e.serialize(serializer),

            ToDeviceEvents::RoomEncrypted(e) => e.serialize(serializer),
            ToDeviceEvents::RoomKey(e) => e.serialize(serializer),
            ToDeviceEvents::RoomKeyRequest(e) => e.serialize(serializer),
            ToDeviceEvents::ForwardedRoomKey(e) => e.serialize(serializer),

            ToDeviceEvents::SecretSend(e) => e.serialize(serializer),
            ToDeviceEvents::SecretRequest(e) => e.serialize(serializer),
            ToDeviceEvents::RoomKeyWithheld(e) => e.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod test {
    use assert_matches::assert_matches;
    use serde_json::{json, Value};

    use super::ToDeviceEvents;

    fn custom_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "custom_key": "custom_value",
            },
            "m.custom.top": "something custom in the top",
            "type": "m.custom.event",
        })
    }

    fn key_verification_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "from_device": "AliceDevice2",
                "methods": [
                  "m.sas.v1"
                ],
                "timestamp": 1559598944869u64,
                "transaction_id": "S0meUniqueAndOpaqueString"
            },
            "type": "m.key.verification.request"
        })
    }

    fn dummy_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {},
            "type": "m.dummy"
        })
    }

    fn secret_request_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "name": "org.example.some.secret",
                "action": "request",
                "requesting_device_id": "ABCDEFG",
                "request_id": "randomly_generated_id_9573"
            },
            "type": "m.secret.request"
        })
    }

    fn forwarded_room_key_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
            "algorithm": "m.megolm.v1.aes-sha2",
            "forwarding_curve25519_key_chain": [
                "hPQNcabIABgGnx3/ACv/jmMmiQHoeFfuLB17tzWp6Hw"
            ],
            "room_id": "!Cuyf34gef24t:localhost",
            "sender_claimed_ed25519_key": "aj40p+aw64yPIdsxoog8jhPu9i7l7NcFRecuOQblE3Y",
            "sender_key": "RF3s+E7RkTQTGF2d8Deol0FkQvgII2aJDf3/Jp5mxVU",
            "session_id": "X3lUlvLELLYxeTx4yOVu6UDpasGEVO0Jbu+QFnm0cKQ",
            "session_key": "AQAAAAq2JpkMceK5f6JrZPJWwzQTn59zliuIv0F7apVLXDcZCCT\
                            3LqBjD21sULYEO5YTKdpMVhi9i6ZSZhdvZvp//tzRpDT7wpWVWI\
                            00Y3EPEjmpm/HfZ4MMAKpk+tzJVuuvfAcHBZgpnxBGzYOc/DAqa\
                            pK7Tk3t3QJ1UMSD94HfAqlb1JF5QBPwoh0fOvD8pJdanB8zxz05\
                            tKFdR73/vo2Q/zE3"
            },
            "type": "m.forwarded_room_key"
        })
    }

    fn room_key_request_event() -> Value {
        json!({
            "sender": "@alice:example.org",
            "content": {
                "action": "request",
                "body": {
                  "algorithm": "m.megolm.v1.aes-sha2",
                  "room_id": "!Cuyf34gef24t:localhost",
                  "sender_key": "RF3s+E7RkTQTGF2d8Deol0FkQvgII2aJDf3/Jp5mxVU",
                  "session_id": "X3lUlvLELLYxeTx4yOVu6UDpasGEVO0Jbu+QFnm0cKQ"
                },
                "request_id": "1495474790150.19",
                "requesting_device_id": "RJYKSTBOIE"
            },
            "type": "m.room_key_request"
        })
    }

    #[test]
    fn deserialization() -> Result<(), serde_json::Error> {
        macro_rules! assert_serialization_roundtrip {
            ( $( $json:path => $to_device_events:ident ),* $(,)? ) => {
                $(
                    let json = $json();
                    let event: ToDeviceEvents = serde_json::from_value(json.clone())?;

                    assert_matches!(event, ToDeviceEvents::$to_device_events(_));
                    let serialized = serde_json::to_value(event)?;
                    assert_eq!(json, serialized);
                )*
            }
        }

        assert_serialization_roundtrip!(
            // `m.room_key
            crate::types::events::room_key::test::json => RoomKey,

            // `m.forwarded_room_key`
            forwarded_room_key_event => ForwardedRoomKey,

            // `m.room_key_request`
            room_key_request_event => RoomKeyRequest,

            // `m.secret.send`
            crate::types::events::secret_send::test::json => SecretSend,

            // `m.secret.request`
            secret_request_event => SecretRequest,

            // Unknown event
            custom_event => Custom,

            // `m.key.verification.request`
            key_verification_event => KeyVerificationRequest,

            // `m.dummy`
            dummy_event => Dummy,

            // `m.room.encrypted`
            crate::types::events::room::encrypted::test::to_device_json => RoomEncrypted,
        );

        Ok(())
    }

    #[test]
    fn zeroized_deserialization() -> Result<(), serde_json::Error> {
        use ruma::events::AnyToDeviceEvent;

        macro_rules! assert_serialization_roundtrip {
            ( $( $json:path => $to_device_events:ident ),* $(,)? ) => {
                $(
                    let json = $json();
                    let event: ToDeviceEvents = serde_json::from_value(json.clone())?;
                    let zeroized = event.serialize_zeroized()?;

                    let ruma_event: AnyToDeviceEvent = zeroized.deserialize_as()?;

                    assert_matches!(ruma_event, AnyToDeviceEvent::$to_device_events(_));
                )*
            }
        }

        assert_serialization_roundtrip!(
            // `m.room_key
            crate::types::events::room_key::test::json => RoomKey,

            // `m.forwarded_room_key`
            forwarded_room_key_event => ForwardedRoomKey,

            // `m.room_key_request`
            room_key_request_event => RoomKeyRequest,

            // `m.secret.send`
            crate::types::events::secret_send::test::json => SecretSend,

            // `m.secret.request`
            secret_request_event => SecretRequest,

            // `m.key.verification.request`
            key_verification_event => KeyVerificationRequest,

            // `m.dummy`
            dummy_event => Dummy,

            // `m.room.encrypted`
            crate::types::events::room::encrypted::test::to_device_json => RoomEncrypted,
        );

        Ok(())
    }
}
