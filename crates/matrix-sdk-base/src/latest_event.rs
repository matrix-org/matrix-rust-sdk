//! Utilities for working with events to decide whether they are suitable for
//! use as a [crate::Room::latest_event].

use matrix_sdk_common::deserialized_responses::TimelineEvent;
#[cfg(feature = "e2e-encryption")]
use ruma::{
    events::{
        call::{invite::SyncCallInviteEvent, notify::SyncCallNotifyEvent},
        poll::unstable_start::SyncUnstablePollStartEvent,
        relation::RelationType,
        room::{
            member::{MembershipState, SyncRoomMemberEvent},
            message::{MessageType, SyncRoomMessageEvent},
            power_levels::RoomPowerLevels,
        },
        sticker::SyncStickerEvent,
        AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent,
    },
    UserId,
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
    /// This message is suitable - it is a sticker
    YesSticker(&'a SyncStickerEvent),
    /// This message is suitable - it is a poll
    YesPoll(&'a SyncUnstablePollStartEvent),

    /// This message is suitable - it is a call invite
    YesCallInvite(&'a SyncCallInviteEvent),

    /// This message is suitable - it's a call notification
    YesCallNotify(&'a SyncCallNotifyEvent),

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

        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::CallNotify(notify)) => {
            PossibleLatestEvent::YesCallNotify(notify)
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
            if let AnySyncStateEvent::RoomMember(member) = state {
                if matches!(member.membership(), MembershipState::Knock) {
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

        Err(serde::de::Error::custom(
            format!("data did not match any variant of serialized LatestEvent (using serde_json). Observed errors: {variant_errors:?}")
        ))
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

    #[cfg(feature = "e2e-encryption")]
    use assert_matches::assert_matches;
    #[cfg(feature = "e2e-encryption")]
    use assert_matches2::assert_let;
    use matrix_sdk_common::deserialized_responses::TimelineEvent;
    use ruma::serde::Raw;
    #[cfg(feature = "e2e-encryption")]
    use ruma::{
        events::{
            call::{
                invite::{CallInviteEventContent, SyncCallInviteEvent},
                notify::{
                    ApplicationType, CallNotifyEventContent, NotifyType, SyncCallNotifyEvent,
                },
                SessionDescription,
            },
            poll::{
                unstable_response::{
                    SyncUnstablePollResponseEvent, UnstablePollResponseEventContent,
                },
                unstable_start::{
                    NewUnstablePollStartEventContent, SyncUnstablePollStartEvent,
                    UnstablePollAnswer, UnstablePollStartContentBlock,
                },
            },
            relation::Replacement,
            room::{
                encrypted::{
                    EncryptedEventScheme, OlmV1Curve25519AesSha2Content, RoomEncryptedEventContent,
                    SyncRoomEncryptedEvent,
                },
                message::{
                    ImageMessageEventContent, MessageType, RedactedRoomMessageEventContent,
                    Relation, RoomMessageEventContent, SyncRoomMessageEvent,
                },
                topic::{RoomTopicEventContent, SyncRoomTopicEvent},
                ImageInfo, MediaSource,
            },
            sticker::{StickerEventContent, SyncStickerEvent},
            AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, EmptyStateKey,
            Mentions, MessageLikeUnsigned, OriginalSyncMessageLikeEvent, OriginalSyncStateEvent,
            RedactedSyncMessageLikeEvent, RedactedUnsigned, StateUnsigned, SyncMessageLikeEvent,
            UnsignedRoomRedactionEvent,
        },
        owned_event_id, owned_mxc_uri, owned_user_id, MilliSecondsSinceUnixEpoch, UInt,
        VoipVersionId,
    };
    use serde_json::json;

    use super::LatestEvent;
    #[cfg(feature = "e2e-encryption")]
    use super::{is_suitable_for_latest_event, PossibleLatestEvent};

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_room_messages_are_suitable() {
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
                is_suitable_for_latest_event(&event, None)
        );

        assert_eq!(m.content.msgtype.msgtype(), "m.image");
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_polls_are_suitable() {
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
                is_suitable_for_latest_event(&event, None)
        );

        assert_eq!(m.content.poll_start().question.text, "do you like rust?");
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_call_invites_are_suitable() {
        let event = AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::CallInvite(
            SyncCallInviteEvent::Original(OriginalSyncMessageLikeEvent {
                content: CallInviteEventContent::new(
                    "call_id".into(),
                    UInt::new(123).unwrap(),
                    SessionDescription::new("".into(), "".into()),
                    VoipVersionId::V1,
                ),
                event_id: owned_event_id!("$1"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                unsigned: MessageLikeUnsigned::new(),
            }),
        ));
        assert_let!(
            PossibleLatestEvent::YesCallInvite(SyncMessageLikeEvent::Original(_)) =
                is_suitable_for_latest_event(&event, None)
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_call_notifications_are_suitable() {
        let event = AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::CallNotify(
            SyncCallNotifyEvent::Original(OriginalSyncMessageLikeEvent {
                content: CallNotifyEventContent::new(
                    "call_id".into(),
                    ApplicationType::Call,
                    NotifyType::Ring,
                    Mentions::new(),
                ),
                event_id: owned_event_id!("$1"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                unsigned: MessageLikeUnsigned::new(),
            }),
        ));
        assert_let!(
            PossibleLatestEvent::YesCallNotify(SyncMessageLikeEvent::Original(_)) =
                is_suitable_for_latest_event(&event, None)
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_stickers_are_suitable() {
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
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::YesSticker(SyncStickerEvent::Original(_))
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_different_types_of_messagelike_are_unsuitable() {
        let event =
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::UnstablePollResponse(
                SyncUnstablePollResponseEvent::Original(OriginalSyncMessageLikeEvent {
                    content: UnstablePollResponseEventContent::new(
                        vec![String::from("option1")],
                        owned_event_id!("$1"),
                    ),
                    event_id: owned_event_id!("$2"),
                    sender: owned_user_id!("@a:b.c"),
                    origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                    unsigned: MessageLikeUnsigned::new(),
                }),
            ));

        assert_matches!(
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::NoUnsupportedMessageLikeType
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_redacted_messages_are_suitable() {
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
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::YesRoomMessage(SyncMessageLikeEvent::Redacted(_))
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_encrypted_messages_are_unsuitable() {
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

        assert_matches!(
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::NoEncrypted
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_state_events_are_unsuitable() {
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

        let event = AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(OriginalSyncMessageLikeEvent {
                content: event_content,
                event_id: owned_event_id!("$2"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(2123).unwrap()),
                unsigned: MessageLikeUnsigned::new(),
            }),
        ));

        assert_matches!(
            is_suitable_for_latest_event(&event, None),
            PossibleLatestEvent::NoUnsupportedMessageLikeType
        );
    }

    #[cfg(feature = "e2e-encryption")]
    #[test]
    fn test_verification_requests_are_unsuitable() {
        use ruma::{device_id, events::room::message::KeyVerificationRequestEventContent, user_id};

        let event = AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(OriginalSyncMessageLikeEvent {
                content: RoomMessageEventContent::new(MessageType::VerificationRequest(
                    KeyVerificationRequestEventContent::new(
                        "body".to_owned(),
                        vec![],
                        device_id!("device_id").to_owned(),
                        user_id!("@user_id:example.com").to_owned(),
                    ),
                )),
                event_id: owned_event_id!("$1"),
                sender: owned_user_id!("@a:b.c"),
                origin_server_ts: MilliSecondsSinceUnixEpoch(UInt::new(123).unwrap()),
                unsigned: MessageLikeUnsigned::new(),
            }),
        ));

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
