//! Utilities for working with events to decide whether they are suitable for
//! use as a [crate::Room::latest_event].

use matrix_sdk_common::deserialized_responses::TimelineEvent;
use ruma::{MilliSecondsSinceUnixEpoch, MxcUri, OwnedEventId};
#[cfg(feature = "e2e-encryption")]
use ruma::{
    UserId,
    events::{
        AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent,
        call::invite::SyncCallInviteEvent,
        poll::unstable_start::SyncUnstablePollStartEvent,
        relation::RelationType,
        room::{
            member::{MembershipState, SyncRoomMemberEvent},
            message::{MessageType, SyncRoomMessageEvent},
            power_levels::RoomPowerLevels,
        },
        rtc::notification::SyncRtcNotificationEvent,
        sticker::SyncStickerEvent,
    },
};
use serde::{Deserialize, Serialize};

use crate::{MinimalRoomMemberEvent, store::SerializableEventContent};

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

    /// The latest event represents a local event that cannot be sent, either
    /// because a previous local event, or this local event cannot be sent.
    LocalCannotBeSent(LocalLatestEventValue),
}

impl LatestEventValue {
    /// Get the timestamp of the [`LatestEventValue`].
    ///
    /// If it's [`None`], it returns `None`. If it's [`Remote`], it returns the
    /// [`TimelineEvent::timestamp`]. If it's [`LocalIsSending`] or
    /// [`LocalCannotBeSent`], it returns the
    /// [`LocalLatestEventValue::timestamp`] value.
    ///
    /// [`None`]: LatestEventValue::None
    /// [`Remote`]: LatestEventValue::Remote
    /// [`LocalIsSending`]: LatestEventValue::LocalIsSending
    /// [`LocalCannotBeSent`]: LatestEventValue::LocalCannotBeSent
    pub fn timestamp(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        match self {
            Self::None => None,
            Self::Remote(remote_latest_event_value) => remote_latest_event_value.timestamp(),
            Self::LocalIsSending(LocalLatestEventValue { timestamp, .. })
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
            Self::LocalIsSending(_) | Self::LocalCannotBeSent(_) => true,
            Self::None | Self::Remote(_) => false,
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
            content: SerializableEventContent::from_raw(
                Raw::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
                "m.room.message".to_owned(),
            ),
        });

        assert_eq!(value.timestamp(), Some(MilliSecondsSinceUnixEpoch(uint!(42))));
    }

    #[test]
    fn test_timestamp_with_local_cannot_be_sent() {
        let value = LatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::from_raw(
                Raw::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
                "m.room.message".to_owned(),
            ),
        });

        assert_eq!(value.timestamp(), Some(MilliSecondsSinceUnixEpoch(uint!(42))));
    }
}

/// Represents a decision about whether an event could be stored as the latest
/// event in a room. Variants starting with Yes indicate that this message could
/// be stored, and provide the inner event information, and those starting with
/// a No indicate that it could not, and give a reason.
#[cfg(feature = "e2e-encryption")]
#[derive(Debug)]
pub enum PossibleLatestEvent<'a> {
    /// This message is suitable - it is an m.room.message
    YesRoomMessage(&'a SyncRoomMessageEvent),
    /// This message is suitable - it is a sticker
    YesSticker(&'a SyncStickerEvent),
    /// This message is suitable - it is a poll
    YesPoll(&'a SyncUnstablePollStartEvent),

    /// This message is suitable - it is a call invite
    YesCallInvite(&'a SyncCallInviteEvent),

    /// This message is suitable - it's a call notification
    YesRtcNotification(&'a SyncRtcNotificationEvent),

    /// This state event is suitable - it's a knock membership change
    /// that can be handled by the current user.
    YesKnockedStateEvent(&'a SyncRoomMemberEvent),

    // Later: YesState(),
    // Later: YesReaction(),
    /// Not suitable - it's a state event
    NoUnsupportedEventType,
    /// Not suitable - it's not a m.room.message or an edit/replacement
    NoUnsupportedMessageLikeType,
    /// Not suitable - it's encrypted
    NoEncrypted,
}

/// Decide whether an event could be stored as the latest event in a room.
/// Returns a LatestEvent representing our decision.
#[cfg(feature = "e2e-encryption")]
pub fn is_suitable_for_latest_event<'a>(
    event: &'a AnySyncTimelineEvent,
    power_levels_info: Option<(&'a UserId, &'a RoomPowerLevels)>,
) -> PossibleLatestEvent<'a> {
    match event {
        // Suitable - we have an m.room.message that was not redacted or edited
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(message)) => {
            if let Some(original_message) = message.as_original() {
                // Don't show incoming verification requests
                if let MessageType::VerificationRequest(_) = original_message.content.msgtype {
                    return PossibleLatestEvent::NoUnsupportedMessageLikeType;
                }

                // Check if this is a replacement for another message. If it is, ignore it
                let is_replacement =
                    original_message.content.relates_to.as_ref().is_some_and(|relates_to| {
                        if let Some(relation_type) = relates_to.rel_type() {
                            relation_type == RelationType::Replacement
                        } else {
                            false
                        }
                    });

                if is_replacement {
                    PossibleLatestEvent::NoUnsupportedMessageLikeType
                } else {
                    PossibleLatestEvent::YesRoomMessage(message)
                }
            } else {
                PossibleLatestEvent::YesRoomMessage(message)
            }
        }

        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::UnstablePollStart(poll)) => {
            PossibleLatestEvent::YesPoll(poll)
        }

        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::CallInvite(invite)) => {
            PossibleLatestEvent::YesCallInvite(invite)
        }

        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RtcNotification(notify)) => {
            PossibleLatestEvent::YesRtcNotification(notify)
        }

        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::Sticker(sticker)) => {
            PossibleLatestEvent::YesSticker(sticker)
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

        // We don't currently support most state events
        AnySyncTimelineEvent::State(state) => {
            // But we make an exception for knocked state events *if* the current user
            // can either accept or decline them
            if let AnySyncStateEvent::RoomMember(member) = state
                && matches!(member.membership(), MembershipState::Knock)
            {
                let can_accept_or_decline_knocks = match power_levels_info {
                    Some((own_user_id, room_power_levels)) => {
                        room_power_levels.user_can_invite(own_user_id)
                            || room_power_levels.user_can_kick(own_user_id)
                    }
                    _ => false,
                };

                // The current user can act on the knock changes, so they should be
                // displayed
                if can_accept_or_decline_knocks {
                    return PossibleLatestEvent::YesKnockedStateEvent(member);
                }
            }
            PossibleLatestEvent::NoUnsupportedEventType
        }
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
    event: TimelineEvent,

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
    event: TimelineEvent,

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

        match serde_json::from_str::<TimelineEvent>(raw.get()) {
            Ok(value) => {
                return Ok(LatestEvent {
                    event: value,
                    sender_profile: None,
                    sender_name_is_ambiguous: None,
                });
            }
            Err(err) => variant_errors.push(err),
        }

        Err(serde::de::Error::custom(format!(
            "data did not match any variant of serialized LatestEvent (using serde_json). \
             Observed errors: {variant_errors:?}"
        )))
    }
}

impl LatestEvent {
    /// Create a new [`LatestEvent`] without the sender's profile.
    pub fn new(event: TimelineEvent) -> Self {
        Self { event, sender_profile: None, sender_name_is_ambiguous: None }
    }

    /// Create a new [`LatestEvent`] with maybe the sender's profile.
    pub fn new_with_sender_details(
        event: TimelineEvent,
        sender_profile: Option<MinimalRoomMemberEvent>,
        sender_name_is_ambiguous: Option<bool>,
    ) -> Self {
        Self { event, sender_profile, sender_name_is_ambiguous }
    }

    /// Transform [`Self`] into an event.
    pub fn into_event(self) -> TimelineEvent {
        self.event
    }

    /// Get a reference to the event.
    pub fn event(&self) -> &TimelineEvent {
        &self.event
    }

    /// Get a mutable reference to the event.
    pub fn event_mut(&mut self) -> &mut TimelineEvent {
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
    #[cfg(feature = "e2e-encryption")]
    use std::collections::BTreeMap;
    use std::time::Duration;

    #[cfg(feature = "e2e-encryption")]
    use assert_matches::assert_matches;
    #[cfg(feature = "e2e-encryption")]
    use assert_matches2::assert_let;
    use matrix_sdk_common::deserialized_responses::TimelineEvent;
    #[cfg(feature = "e2e-encryption")]
    use matrix_sdk_test::event_factory::EventFactory;
    #[cfg(feature = "e2e-encryption")]
    use ruma::{
        MilliSecondsSinceUnixEpoch, UInt, VoipVersionId,
        events::{
            SyncMessageLikeEvent,
            call::{SessionDescription, invite::CallInviteEventContent},
            poll::{
                unstable_response::UnstablePollResponseEventContent,
                unstable_start::{
                    NewUnstablePollStartEventContent, UnstablePollAnswer,
                    UnstablePollStartContentBlock,
                },
            },
            relation::Replacement,
            room::{
                ImageInfo, MediaSource,
                encrypted::{
                    EncryptedEventScheme, OlmV1Curve25519AesSha2Content, RoomEncryptedEventContent,
                },
                message::{
                    ImageMessageEventContent, MessageType, RedactedRoomMessageEventContent,
                    Relation, RoomMessageEventContent,
                },
                topic::RoomTopicEventContent,
            },
            sticker::{StickerEventContent, SyncStickerEvent},
        },
        owned_event_id, owned_mxc_uri, user_id,
    };
    use ruma::{
        events::rtc::notification::{NotificationType, RtcNotificationEventContent},
        serde::Raw,
    };
    use serde_json::json;

    use super::LatestEvent;
    #[cfg(feature = "e2e-encryption")]
    use super::{PossibleLatestEvent, is_suitable_for_latest_event};

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_room_messages_are_suitable() {
        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .event(RoomMessageEventContent::new(MessageType::Image(ImageMessageEventContent::new(
                "".to_owned(),
                MediaSource::Plain(owned_mxc_uri!("mxc://example.com/1")),
            ))))
            .into();
        assert_let!(
            PossibleLatestEvent::YesRoomMessage(SyncMessageLikeEvent::Original(m)) =
                is_suitable_for_latest_event(&event, None)
        );

        assert_eq!(m.content.msgtype.msgtype(), "m.image");
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_polls_are_suitable() {
        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .event(NewUnstablePollStartEventContent::new(UnstablePollStartContentBlock::new(
                "do you like rust?",
                vec![UnstablePollAnswer::new("id", "yes")].try_into().unwrap(),
            )))
            .into();
        assert_let!(
            PossibleLatestEvent::YesPoll(SyncMessageLikeEvent::Original(m)) =
                is_suitable_for_latest_event(&event, None)
        );

        assert_eq!(m.content.poll_start().question.text, "do you like rust?");
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_call_invites_are_suitable() {
        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .event(CallInviteEventContent::new(
                "call_id".into(),
                UInt::new(123).unwrap(),
                SessionDescription::new("".into(), "".into()),
                VoipVersionId::V1,
            ))
            .into();
        assert_let!(
            PossibleLatestEvent::YesCallInvite(SyncMessageLikeEvent::Original(_)) =
                is_suitable_for_latest_event(&event, None)
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_call_notifications_are_suitable() {
        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .event(RtcNotificationEventContent::new(
                MilliSecondsSinceUnixEpoch::now(),
                Duration::new(30, 0),
                NotificationType::Ring,
            ))
            .into();
        assert_let!(
            PossibleLatestEvent::YesRtcNotification(SyncMessageLikeEvent::Original(_)) =
                is_suitable_for_latest_event(&event, None)
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_stickers_are_suitable() {
        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .event(StickerEventContent::new(
                "sticker!".to_owned(),
                ImageInfo::new(),
                owned_mxc_uri!("mxc://example.com/1"),
            ))
            .into();

        assert_matches!(
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::YesSticker(SyncStickerEvent::Original(_))
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_different_types_of_messagelike_are_unsuitable() {
        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .event(UnstablePollResponseEventContent::new(
                vec![String::from("option1")],
                owned_event_id!("$1"),
            ))
            .into();

        assert_matches!(
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::NoUnsupportedMessageLikeType
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_redacted_messages_are_suitable() {
        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .redacted(user_id!("@x:y.za"), RedactedRoomMessageEventContent::new())
            .into();

        assert_matches!(
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::YesRoomMessage(SyncMessageLikeEvent::Redacted(_))
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_encrypted_messages_are_unsuitable() {
        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .event(RoomEncryptedEventContent::new(
                EncryptedEventScheme::OlmV1Curve25519AesSha2(OlmV1Curve25519AesSha2Content::new(
                    BTreeMap::new(),
                    "".to_owned(),
                )),
                None,
            ))
            .into();

        assert_matches!(
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::NoEncrypted
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_state_events_are_unsuitable() {
        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .event(RoomTopicEventContent::new("".to_owned()))
            .state_key("")
            .into();

        assert_matches!(
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::NoUnsupportedEventType
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_replacement_events_are_unsuitable() {
        let mut event_content = RoomMessageEventContent::text_plain("Bye bye, world!");
        event_content.relates_to = Some(Relation::Replacement(Replacement::new(
            owned_event_id!("$1"),
            RoomMessageEventContent::text_plain("Hello, world!").into(),
        )));

        let event = EventFactory::new().sender(user_id!("@a:b.c")).event(event_content).into();

        assert_matches!(
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::NoUnsupportedMessageLikeType
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_verification_requests_are_unsuitable() {
        use ruma::{device_id, events::room::message::KeyVerificationRequestEventContent, user_id};

        let event = EventFactory::new()
            .sender(user_id!("@a:b.c"))
            .event(RoomMessageEventContent::new(MessageType::VerificationRequest(
                KeyVerificationRequestEventContent::new(
                    "body".to_owned(),
                    vec![],
                    device_id!("device_id").to_owned(),
                    user_id!("@user_id:example.com").to_owned(),
                ),
            )))
            .into();

        assert_let!(
            PossibleLatestEvent::NoUnsupportedMessageLikeType =
                is_suitable_for_latest_event(&event, None)
        );
    }

    #[test]
    fn test_deserialize_latest_event() {
        #[derive(Debug, serde::Serialize, serde::Deserialize)]
        struct TestStruct {
            latest_event: LatestEvent,
        }

        let event = TimelineEvent::from_plaintext(
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
                        "kind": {
                            "PlainText": {
                                "event": {
                                    "event_id": "$1"
                                }
                            }
                        },
                        "thread_summary": "None",
                        "timestamp": null,
                    }
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
                "latest_event": {
                    "event": {
                        "encryption_info": null,
                        "event": {
                            "event_id": "$1"
                        }
                    },
                }
        });

        let deserialized: TestStruct = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized.latest_event.event().event_id().unwrap(), "$1");
        assert!(deserialized.latest_event.sender_profile.is_none());
        assert!(deserialized.latest_event.sender_name_is_ambiguous.is_none());

        // The even older format can also be deserialized.
        let serialized = json!({
            "latest_event": event
        });

        let deserialized: TestStruct = serde_json::from_value(serialized).unwrap();
        assert_eq!(deserialized.latest_event.event().event_id().unwrap(), "$1");
        assert!(deserialized.latest_event.sender_profile.is_none());
        assert!(deserialized.latest_event.sender_name_is_ambiguous.is_none());
    }
}
