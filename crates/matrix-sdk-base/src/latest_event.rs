//! Utilities for working with events to decide whether they are suitable for
//! use as a [crate::Room::latest_event].

#![cfg(all(feature = "e2e-encryption", feature = "experimental-sliding-sync"))]

use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use ruma::{
    events::{
        poll::unstable_start::SyncUnstablePollStartEvent, room::message::SyncRoomMessageEvent,
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
    },
    OwnedEventId,
};
use serde::{Deserialize, Serialize};

use crate::RoomMember;

/// Represents a decision about whether an event could be stored as the latest
/// event in a room. Variants starting with Yes indicate that this message could
/// be stored, and provide the inner event information, and those starting with
/// a No indicate that it could not, and give a reason.
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

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct LatestEvent {
    event: SyncTimelineEvent,
    // member: Option<RoomMember>,
}

impl LatestEvent {
    pub fn new(event: SyncTimelineEvent) -> Self {
        Self { event }
    }

    // pub fn new_with_member(event: SyncTimelineEvent, member: RoomMember) -> Self
    // {     Self { event, member: Some(member) }
    // }

    pub fn into_event(self) -> SyncTimelineEvent {
        self.event
    }

    pub fn event(&self) -> &SyncTimelineEvent {
        &self.event
    }

    pub fn event_mut(&mut self) -> &mut SyncTimelineEvent {
        &mut self.event
    }

    pub fn event_id(&self) -> Option<OwnedEventId> {
        self.event.event_id()
    }
    // pub fn member(&self) -> Option<&RoomMember> {
    //     self.member.as_ref()
    // }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
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
        owned_event_id, owned_mxc_uri, owned_user_id, MilliSecondsSinceUnixEpoch, UInt,
    };
    use serde_json::json;

    use crate::latest_event::{is_suitable_for_latest_event, PossibleLatestEvent};

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
        let m = assert_matches!(
            is_suitable_for_latest_event(&event),
            PossibleLatestEvent::YesRoomMessage(SyncMessageLikeEvent::Original(m)) => m
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
        let m = assert_matches!(
            is_suitable_for_latest_event(&event),
            PossibleLatestEvent::YesPoll(SyncMessageLikeEvent::Original(m)) => m
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
}
