use std::collections::BTreeMap;

use ruma::{
    events::{AnySyncTimelineEvent, AnyTimelineEvent},
    serde::Raw,
    DeviceKeyAlgorithm, OwnedDeviceId, OwnedEventId, OwnedUserId,
};
use serde::{Deserialize, Serialize};

/// The verification state of the device that sent an event to us.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum VerificationState {
    /// The device is trusted.
    Trusted,
    /// The device is not trusted.
    Untrusted,
    /// The device is not known to us.
    UnknownDevice,
}

/// The algorithm specific information of a decrypted event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum AlgorithmInfo {
    /// The info if the event was encrypted using m.megolm.v1.aes-sha2
    MegolmV1AesSha2 {
        /// The curve25519 key of the device that created the megolm decryption
        /// key originally.
        curve25519_key: String,
        /// The signing keys that have created the megolm key that was used to
        /// decrypt this session. This map will usually contain a single ed25519
        /// key.
        sender_claimed_keys: BTreeMap<DeviceKeyAlgorithm, String>,
    },
}

/// Struct containing information on how an event was decrypted.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncryptionInfo {
    /// The user ID of the event sender, note this is untrusted data unless the
    /// `verification_state` is as well trusted.
    pub sender: OwnedUserId,
    /// The device ID of the device that sent us the event, note this is
    /// untrusted data unless `verification_state` is as well trusted.
    pub sender_device: Option<OwnedDeviceId>,
    /// Information about the algorithm that was used to encrypt the event.
    pub algorithm_info: AlgorithmInfo,
    /// The verification state of the device that sent us the event, note this
    /// is the state of the device at the time of decryption. It may change in
    /// the future if a device gets verified or deleted.
    pub verification_state: VerificationState,
}

/// A customized version of a room event coming from a sync that holds optional
/// encryption info.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SyncTimelineEvent {
    /// The actual event.
    pub event: Raw<AnySyncTimelineEvent>,
    /// The encryption info about the event. Will be `None` if the event was not
    /// encrypted.
    pub encryption_info: Option<EncryptionInfo>,
}

impl SyncTimelineEvent {
    /// Get the event id of this `SyncTimelineEvent` if the event has any valid
    /// id.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.event.get_field::<OwnedEventId>("event_id").ok().flatten()
    }
}

impl From<Raw<AnySyncTimelineEvent>> for SyncTimelineEvent {
    fn from(inner: Raw<AnySyncTimelineEvent>) -> Self {
        Self { encryption_info: None, event: inner }
    }
}

impl From<TimelineEvent> for SyncTimelineEvent {
    fn from(o: TimelineEvent) -> Self {
        // This conversion is unproblematic since a `SyncTimelineEvent` is just a
        // `TimelineEvent` without the `room_id`. By converting the raw value in
        // this way, we simply cause the `room_id` field in the json to be
        // ignored by a subsequent deserialization.
        Self { encryption_info: o.encryption_info, event: o.event.cast() }
    }
}

#[derive(Clone, Debug)]
pub struct TimelineEvent {
    /// The actual event.
    pub event: Raw<AnyTimelineEvent>,
    /// The encryption info about the event. Will be `None` if the event was not
    /// encrypted.
    pub encryption_info: Option<EncryptionInfo>,
}

#[cfg(test)]
mod tests {
    use ruma::{
        events::{room::message::RoomMessageEventContent, AnySyncTimelineEvent},
        serde::Raw,
    };
    use serde_json::json;

    use super::{SyncTimelineEvent, TimelineEvent};

    #[test]
    fn room_event_to_sync_room_event() {
        let event = json! ({
            "content": RoomMessageEventContent::text_plain("foobar"),
            "type": "m.room.message",
            "event_id": "$xxxxx:example.org",
            "room_id": "!someroom:example.com",
            "origin_server_ts": 2189,
            "sender": "@carl:example.com",
        });

        let room_event =
            TimelineEvent { event: Raw::new(&event).unwrap().cast(), encryption_info: None };

        let converted_room_event: SyncTimelineEvent = room_event.into();

        let converted_event: AnySyncTimelineEvent =
            converted_room_event.event.deserialize().unwrap();

        assert_eq!(converted_event.event_id(), "$xxxxx:example.org");
        assert_eq!(converted_event.sender(), "@carl:example.com");
    }
}
