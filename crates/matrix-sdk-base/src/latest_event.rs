//! Utilities for working with events to decide whether they are suitable for
//! use as a [crate::Room::latest_event].

#![cfg(feature = "experimental-sliding-sync")]

use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
#[cfg(feature = "e2e-encryption")]
use ruma::events::{
    poll::unstable_start::SyncUnstablePollStartEvent, room::message::SyncRoomMessageEvent,
    AnySyncMessageLikeEvent, AnySyncTimelineEvent,
};
use ruma::{MxcUri, OwnedEventId};
use serde::{Deserialize, Serialize};

use crate::MinimalRoomMemberEvent;

/// Represents a decision about whether an event could be stored as the latest
/// event in a room. Variants starting with Yes indicate that this message could
/// be stored, and provide the inner event information, and those starting with
/// a No indicate that it could not, and give a reason.
#[cfg(feature = "e2e-encryption")]
#[derive(Debug)]
pub enum PossibleLatestEvent<'a> {
    /// This message is suitable - it is an m.room.message
    YesRoomMessage(&'a SyncRoomMessageEvent),
    /// This message is suitable - it is a poll
    YesPoll(&'a SyncUnstablePollStartEvent),
    // Later: YesState(),
    // Later: YesReaction(),
    /// Not suitable - it's a state event
    NoUnsupportedEventType,
    /// Not suitable - it's not an m.room.message
    NoUnsupportedMessageLikeType,
    /// Not suitable - it's encrypted
    NoEncrypted,
}

/// Decide whether an event could be stored as the latest event in a room.
/// Returns a LatestEvent representing our decision.
#[cfg(feature = "e2e-encryption")]
pub fn is_suitable_for_latest_event(event: &AnySyncTimelineEvent) -> PossibleLatestEvent<'_> {
    match event {
        // Suitable - we have an m.room.message that was not redacted
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(message)) => {
            PossibleLatestEvent::YesRoomMessage(message)
        }

        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::UnstablePollStart(poll)) => {
            PossibleLatestEvent::YesPoll(poll)
        }

        // Encrypted events are not suitable
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(_)) => {
            PossibleLatestEvent::NoEncrypted
        }

        // Later, if we support reactions:
        // AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Reaction(_))

        // MessageLike, but not one of the types we want to show in message previews, so not
        // suitable
        AnySyncTimelineEvent::MessageLike(_) => PossibleLatestEvent::NoUnsupportedMessageLikeType,

        // We don't currently support state events
        AnySyncTimelineEvent::State(_) => PossibleLatestEvent::NoUnsupportedEventType,
    }
}

/// Represent all information required to represent a latest event in an
/// efficient way.
///
/// ## Implementation details
///
/// Serialization and deserialization should be a breeze, but we introduced a
/// change in the format without realizing, and without a migration. Ideally,
/// this would be handled with a `serde(untagged)` enum that would be used to
/// deserialize in either the older format, or to the new format. Unfortunately,
/// untagged enums don't play nicely with `serde_json::value::RawValue`,
/// so we did have to implement a custom `Deserialize` for `LatestEvent`, that
/// first deserializes the thing as a raw JSON value, and then deserializes the
/// JSON string as one variant or the other.
///
/// Because of that, `LatestEvent` should only be (de)serialized using
/// serde_json.
///
/// Whenever you introduce new fields to `LatestEvent` make sure to add them to
/// `SerializedLatestEvent` too.
#[derive(Clone, Debug, Serialize)]
pub struct LatestEvent {
    /// The actual event.
    event: SyncTimelineEvent,

    /// The member profile of the event' sender.
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_profile: Option<MinimalRoomMemberEvent>,

    /// The name of the event' sender is ambiguous.
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_name_is_ambiguous: Option<bool>,
}

#[derive(Deserialize)]
struct SerializedLatestEvent {
    /// The actual event.
    event: SyncTimelineEvent,

    /// The member profile of the event' sender.
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_profile: Option<MinimalRoomMemberEvent>,

    /// The name of the event' sender is ambiguous.
    #[serde(skip_serializing_if = "Option::is_none")]
    sender_name_is_ambiguous: Option<bool>,
}

// Note: this deserialize implementation for LatestEvent will *only* work with
// serde_json.
impl<'de> Deserialize<'de> for LatestEvent {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw: Box<serde_json::value::RawValue> = Box::deserialize(deserializer)?;

        let mut variant_errors = Vec::new();

        match serde_json::from_str::<SerializedLatestEvent>(raw.get()) {
            Ok(value) => {
                return Ok(LatestEvent {
                    event: value.event,
                    sender_profile: value.sender_profile,
                    sender_name_is_ambiguous: value.sender_name_is_ambiguous,
                });
            }
            Err(err) => variant_errors.push(err),
        }

        match serde_json::from_str::<SyncTimelineEvent>(raw.get()) {
            Ok(value) => {
                return Ok(LatestEvent {
                    event: value,
                    sender_profile: None,
                    sender_name_is_ambiguous: None,
                })
            }
            Err(err) => variant_errors.push(err),
        }

        Err(serde::de::Error::custom(
            format!("data did not match any variant of serialized LatestEvent (using serde_json). Observed errors: {variant_errors:?}")
        ))
    }
}

impl LatestEvent {
    /// Create a new [`LatestEvent`] without the sender's profile.
    pub fn new(event: SyncTimelineEvent) -> Self {
        Self { event, sender_profile: None, sender_name_is_ambiguous: None }
    }

    /// Create a new [`LatestEvent`] with maybe the sender's profile.
    pub fn new_with_sender_details(
        event: SyncTimelineEvent,
        sender_profile: Option<MinimalRoomMemberEvent>,
        sender_name_is_ambiguous: Option<bool>,
    ) -> Self {
        Self { event, sender_profile, sender_name_is_ambiguous }
    }

    /// Transform [`Self`] into an event.
    pub fn into_event(self) -> SyncTimelineEvent {
        self.event
    }

    /// Get a reference to the event.
    pub fn event(&self) -> &SyncTimelineEvent {
        &self.event
    }

    /// Get a mutable reference to the event.
    pub fn event_mut(&mut self) -> &mut SyncTimelineEvent {
        &mut self.event
    }

    /// Get the event ID.
    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.event.event_id()
    }

    /// Check whether [`Self`] has a sender profile.
    pub fn has_sender_profile(&self) -> bool {
        self.sender_profile.is_some()
    }

    /// Return the sender's display name if it was known at the time [`Self`]
    /// was built.
    pub fn sender_display_name(&self) -> Option<&str> {
        self.sender_profile.as_ref().and_then(|profile| {
            profile.as_original().and_then(|event| event.content.displayname.as_deref())
        })
    }

    /// Return `Some(true)` if the sender's name is ambiguous, `Some(false)` if
    /// it isn't, `None` if ambiguity detection wasn't possible at the time
    /// [`Self`] was built.
    pub fn sender_name_ambiguous(&self) -> Option<bool> {
        self.sender_name_is_ambiguous
    }

    /// Return the sender's avatar URL if it was known at the time [`Self`] was
    /// built.
    pub fn sender_avatar_url(&self) -> Option<&MxcUri> {
        self.sender_profile.as_ref().and_then(|profile| {
            profile.as_original().and_then(|event| event.content.avatar_url.as_deref())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
    use ruma::{
        events::{
            poll::unstable_start::{
                NewUnstablePollStartEventContent, SyncUnstablePollStartEvent, UnstablePollAnswer,
                UnstablePollStartContentBlock,
            },
            room::{
                encrypted::{
                    EncryptedEventScheme, OlmV1Curve25519AesSha2Content, RoomEncryptedEventContent,
                    SyncRoomEncryptedEvent,
                },
                message::{
                    ImageMessageEventContent, MessageType, RedactedRoomMessageEventContent,
                    RoomMessageEventContent, SyncRoomMessageEvent,
                },
                topic::{RoomTopicEventContent, SyncRoomTopicEvent},
                ImageInfo, MediaSource,
            },
            sticker::{StickerEventContent, SyncStickerEvent},
            AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, EmptyStateKey,
            MessageLikeUnsigned, OriginalSyncMessageLikeEvent, OriginalSyncStateEvent,
            RedactedSyncMessageLikeEvent, RedactedUnsigned, StateUnsigned, SyncMessageLikeEvent,
            UnsignedRoomRedactionEvent,
        },
        owned_event_id, owned_mxc_uri, owned_user_id,
        serde::Raw,
        MilliSecondsSinceUnixEpoch, UInt,
    };
    use serde_json::json;

    use crate::latest_event::{is_suitable_for_latest_event, LatestEvent, PossibleLatestEvent};

    #[test]
    fn room_messages_are_suitable() {
        let event = AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(OriginalSyncMessageLikeEvent {
                content: RoomMessageEventContent::new(MessageType::Image(
                    ImageMessageEventContent::new(
                        "".to_owned(),
                        MediaSource::Plain(owned_mxc_uri!("mxc://example.com/1")),
                    ),
                )),
                event_id: owned_event_id!("$1"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                unsigned: MessageLikeUnsigned::new(),
            }),
        ));
        assert_let!(
            PossibleLatestEvent::YesRoomMessage(SyncMessageLikeEvent::Original(m)) =
                is_suitable_for_latest_event(&event)
        );

        assert_eq!(m.content.msgtype.msgtype(), "m.image");
    }

    #[test]
    fn polls_are_suitable() {
        let event = AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::UnstablePollStart(
            SyncUnstablePollStartEvent::Original(OriginalSyncMessageLikeEvent {
                content: NewUnstablePollStartEventContent::new(UnstablePollStartContentBlock::new(
                    "do you like rust?",
                    vec![UnstablePollAnswer::new("id", "yes")].try_into().unwrap(),
                ))
                .into(),
                event_id: owned_event_id!("$1"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                unsigned: MessageLikeUnsigned::new(),
            }),
        ));
        assert_let!(
            PossibleLatestEvent::YesPoll(SyncMessageLikeEvent::Original(m)) =
                is_suitable_for_latest_event(&event)
        );

        assert_eq!(m.content.poll_start().question.text, "do you like rust?");
    }

    #[test]
    fn different_types_of_messagelike_are_unsuitable() {
        let event = AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Sticker(
            SyncStickerEvent::Original(OriginalSyncMessageLikeEvent {
                content: StickerEventContent::new(
                    "sticker!".to_owned(),
                    ImageInfo::new(),
                    owned_mxc_uri!("mxc://example.com/1"),
                ),
                event_id: owned_event_id!("$1"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                unsigned: MessageLikeUnsigned::new(),
            }),
        ));

        assert_matches!(
            is_suitable_for_latest_event(&event),
            PossibleLatestEvent::NoUnsupportedMessageLikeType
        );
    }

    #[test]
    fn redacted_messages_are_suitable() {
        // Ruma does not allow constructing UnsignedRoomRedactionEvent instances.
        let room_redaction_event: UnsignedRoomRedactionEvent = serde_json::from_value(json!({
            "content": {},
            "event_id": "$redaction",
            "sender": "@x:y.za",
            "origin_server_ts": 223543,
            "unsigned": { "reason": "foo" }
        }))
        .unwrap();

        let event = AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Redacted(RedactedSyncMessageLikeEvent {
                content: RedactedRoomMessageEventContent::new(),
                event_id: owned_event_id!("$1"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                unsigned: RedactedUnsigned::new(room_redaction_event),
            }),
        ));

        assert_matches!(
            is_suitable_for_latest_event(&event),
            PossibleLatestEvent::YesRoomMessage(SyncMessageLikeEvent::Redacted(_))
        );
    }

    #[test]
    fn encrypted_messages_are_unsuitable() {
        let event = AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
            SyncRoomEncryptedEvent::Original(OriginalSyncMessageLikeEvent {
                content: RoomEncryptedEventContent::new(
                    EncryptedEventScheme::OlmV1Curve25519AesSha2(
                        OlmV1Curve25519AesSha2Content::new(BTreeMap::new(), "".to_owned()),
                    ),
                    None,
                ),
                event_id: owned_event_id!("$1"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                unsigned: MessageLikeUnsigned::new(),
            }),
        ));

        assert_matches!(is_suitable_for_latest_event(&event), PossibleLatestEvent::NoEncrypted);
    }

    #[test]
    fn state_events_are_unsuitable() {
        let event = AnySyncTimelineEvent::State(AnySyncStateEvent::RoomTopic(
            SyncRoomTopicEvent::Original(OriginalSyncStateEvent {
                content: RoomTopicEventContent::new("".to_owned()),
                event_id: owned_event_id!("$1"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                unsigned: StateUnsigned::new(),
                state_key: EmptyStateKey,
            }),
        ));

        assert_matches!(
            is_suitable_for_latest_event(&event),
            PossibleLatestEvent::NoUnsupportedEventType
        );
    }

    #[test]
    fn deserialize_latest_event() {
        #[derive(Debug, serde::Serialize, serde::Deserialize)]
        struct TestStruct {
            latest_event: LatestEvent,
        }

        let event = SyncTimelineEvent::new(
            Raw::from_json_string(json!({ "event_id": "$1" }).to_string()).unwrap(),
        );

        let initial = TestStruct {
            latest_event: LatestEvent {
                event: event.clone(),
                sender_profile: None,
                sender_name_is_ambiguous: None,
            },
        };

        // When serialized, LatestEvent always uses the new format.
        let serialized = serde_json::to_value(&initial).unwrap();
        assert_eq!(
            serialized,
            json!({
                "latest_event": {
                    "event": {
                        "encryption_info": null,
                        "event": {
                            "event_id": "$1"
                        }
                    },
                }
            })
        );

        // And it can be properly deserialized from the new format.
        let deserialized: TestStruct = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized.latest_event.event().event_id().unwrap(), "$1");
        assert!(deserialized.latest_event.sender_profile.is_none());
        assert!(deserialized.latest_event.sender_name_is_ambiguous.is_none());

        // The previous format can also be deserialized.
        let serialized = json!({
            "latest_event": event
        });

        let deserialized: TestStruct = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized.latest_event.event().event_id().unwrap(), "$1");
        assert!(deserialized.latest_event.sender_profile.is_none());
        assert!(deserialized.latest_event.sender_name_is_ambiguous.is_none());
    }
}
