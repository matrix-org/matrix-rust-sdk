//! The Latest Event basic types.

use matrix_sdk_common::deserialized_responses::TimelineEvent;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId};
use serde::{Deserialize, Serialize};

use crate::store::SerializableEventContent;

/// A latest event value!
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub enum LatestEventValue {
    /// No value has been computed yet, or no candidate value was found.
    #[default]
    None,

    /// The latest event represents a remote event.
    Remote(RemoteLatestEventValue),

    /// The latest event represents a local event that is sending.
    LocalIsSending(LocalLatestEventValue),

    /// The latest event represents a local event that has been sent
    /// successfully. It should come quickly as a [`Self::Remote`].
    LocalHasBeenSent {
        /// ID of the sent event.
        event_id: OwnedEventId,

        /// Value, as for other `Self::Local*` variants.
        value: LocalLatestEventValue,
    },

    /// The latest event represents a local event that cannot be sent, either
    /// because a previous local event, or this local event cannot be sent.
    LocalCannotBeSent(LocalLatestEventValue),
}

impl LatestEventValue {
    /// Get the timestamp of the [`LatestEventValue`].
    ///
    /// If it's [`None`], it returns `None`. If it's [`Remote`], it returns the
    /// [`TimelineEvent::timestamp`]. If it's [`LocalIsSending`],
    /// [`LocalHasBeenSent`] or [`LocalCannotBeSent`], it returns the
    /// [`LocalLatestEventValue::timestamp`] value.
    ///
    /// [`None`]: LatestEventValue::None
    /// [`Remote`]: LatestEventValue::Remote
    /// [`LocalIsSending`]: LatestEventValue::LocalIsSending
    /// [`LocalHasBeenSent`]: LatestEventValue::LocalHasBeenSent
    /// [`LocalCannotBeSent`]: LatestEventValue::LocalCannotBeSent
    pub fn timestamp(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        match self {
            Self::None => None,
            Self::Remote(remote_latest_event_value) => remote_latest_event_value.timestamp(),
            Self::LocalIsSending(LocalLatestEventValue { timestamp, .. })
            | Self::LocalHasBeenSent { value: LocalLatestEventValue { timestamp, .. }, .. }
            | Self::LocalCannotBeSent(LocalLatestEventValue { timestamp, .. }) => Some(*timestamp),
        }
    }

    /// Check whether the [`LatestEventValue`] represents a local value or not,
    /// i.e. it is [`LocalIsSending`] or [`LocalCannotBeSent`].
    ///
    /// [`LocalIsSending`]: LatestEventValue::LocalIsSending
    /// [`LocalCannotBeSent`]: LatestEventValue::LocalCannotBeSent
    pub fn is_local(&self) -> bool {
        match self {
            Self::LocalIsSending(_)
            | Self::LocalHasBeenSent { .. }
            | Self::LocalCannotBeSent(_) => true,
            Self::None | Self::Remote(_) => false,
        }
    }

    /// Check whether the [`LatestEventValue`] represents an unsent event, i.e.
    /// is [`LocalIsSending`] nor [`LocalCannotBeSent`].
    ///
    /// [`LocalIsSending`]: LatestEventValue::LocalIsSending
    /// [`LocalCannotBeSent`]: LatestEventValue::LocalCannotBeSent
    pub fn is_unsent(&self) -> bool {
        match self {
            Self::LocalIsSending(_) | Self::LocalCannotBeSent(_) => true,
            Self::LocalHasBeenSent { .. } | Self::Remote(_) | Self::None => false,
        }
    }

    /// Check whether the [`LatestEventValue`] is not set, i.e. [`None`].
    ///
    /// [`None`]: LatestEventValue::None
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }

    /// Get the event ID (if it exists) of the event representing the
    /// [`LatestEventValue`].
    pub fn event_id(&self) -> Option<OwnedEventId> {
        match self {
            Self::Remote(event) => event.event_id(),
            Self::LocalHasBeenSent { event_id, .. } => Some(event_id.clone()),
            Self::LocalIsSending(_) | Self::LocalCannotBeSent(_) | Self::None => None,
        }
    }
}

/// Represents the value for [`LatestEventValue::Remote`].
pub type RemoteLatestEventValue = TimelineEvent;

/// Represents the value for [`LatestEventValue::LocalIsSending`] and
/// [`LatestEventValue::LocalCannotBeSent`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalLatestEventValue {
    /// The time where the event has been created (by this module).
    pub timestamp: MilliSecondsSinceUnixEpoch,

    /// The content of the local event.
    pub content: SerializableEventContent,
}

#[cfg(test)]
mod tests_latest_event_value {
    use ruma::{
        MilliSecondsSinceUnixEpoch,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        owned_event_id,
        serde::Raw,
        uint,
    };
    use serde_json::json;

    use super::{LatestEventValue, LocalLatestEventValue, RemoteLatestEventValue};
    use crate::store::SerializableEventContent;

    #[test]
    fn test_timestamp_with_none() {
        let value = LatestEventValue::None;

        assert_eq!(value.timestamp(), None);
    }

    #[test]
    fn test_timestamp_with_remote() {
        let value = LatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain("raclette"),
                    "type": "m.room.message",
                    "event_id": "$ev0",
                    "room_id": "!r0",
                    "origin_server_ts": 42,
                    "sender": "@mnt_io:matrix.org",
                })
                .to_string(),
            )
            .unwrap(),
        ));

        assert_eq!(value.timestamp(), Some(MilliSecondsSinceUnixEpoch(uint!(42))));
    }

    #[test]
    fn test_timestamp_with_local_is_sending() {
        let value = LatestEventValue::LocalIsSending(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        });

        assert_eq!(value.timestamp(), Some(MilliSecondsSinceUnixEpoch(uint!(42))));
    }

    #[test]
    fn test_timestamp_with_local_has_been_sent() {
        let value = LatestEventValue::LocalHasBeenSent {
            event_id: owned_event_id!("$ev0"),
            value: LocalLatestEventValue {
                timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
                content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
            },
        };

        assert_eq!(value.timestamp(), Some(MilliSecondsSinceUnixEpoch(uint!(42))));
    }

    #[test]
    fn test_timestamp_with_local_cannot_be_sent() {
        let value = LatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        });

        assert_eq!(value.timestamp(), Some(MilliSecondsSinceUnixEpoch(uint!(42))));
    }

    #[test]
    fn test_event_id_with_none() {
        let value = LatestEventValue::None;

        assert!(value.event_id().is_none());
    }

    #[test]
    fn test_event_id_with_remote() {
        let event_id = owned_event_id!("$ev0");
        let value = LatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain("raclette"),
                    "type": "m.room.message",
                    "event_id": event_id,
                    "room_id": "!r0",
                    "origin_server_ts": 42,
                    "sender": "@mnt_io:matrix.org",
                })
                .to_string(),
            )
            .unwrap(),
        ));

        assert_eq!(value.event_id(), Some(event_id));
    }

    #[test]
    fn test_event_id_with_local_is_sending() {
        let value = LatestEventValue::LocalIsSending(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        });

        assert!(value.event_id().is_none());
    }

    #[test]
    fn test_event_id_with_local_has_been_sent() {
        let event_id = owned_event_id!("$ev0");
        let value = LatestEventValue::LocalHasBeenSent {
            event_id: event_id.clone(),
            value: LocalLatestEventValue {
                timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
                content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
            },
        };

        assert_eq!(value.event_id(), Some(event_id));
    }

    #[test]
    fn test_event_id_with_local_cannot_be_sent() {
        let value = LatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        });

        assert!(value.event_id().is_none());
    }
}
