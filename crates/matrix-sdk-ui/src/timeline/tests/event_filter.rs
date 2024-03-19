// Copyright 2023 Kévin Commaille
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

use std::sync::Arc;

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk_test::{async_test, sync_timeline_event, ALICE, BOB};
use ruma::{
    assign,
    events::{
        reaction::ReactionEventContent,
        relation::{Annotation, Replacement},
        room::{
            member::{MembershipState, RoomMemberEventContent},
            message::{
                MessageType, RedactedRoomMessageEventContent, Relation, RoomMessageEventContent,
            },
            name::RoomNameEventContent,
            topic::RoomTopicEventContent,
        },
        AnySyncTimelineEvent, TimelineEventType,
    },
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::{
    inner::TimelineInnerSettings, AnyOtherFullStateEventContent, TimelineEventTypeFilter,
    TimelineItem, TimelineItemContent, TimelineItemKind,
};

#[async_test]
async fn test_default_filter() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // Test edits work.
    timeline
        .handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("The first message"))
        .await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    let first_event_id = item.as_event().unwrap().event_id().unwrap();

    let edit = assign!(RoomMessageEventContent::text_plain(" * The _edited_ first message"), {
        relates_to: Some(Relation::Replacement(Replacement::new(
            first_event_id.to_owned(),
            MessageType::text_plain("The _edited_ first message").into(),
        ))),
    });
    timeline.handle_live_message_event(&ALICE, edit).await;

    // The edit was applied.
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    assert_let!(TimelineItemContent::Message(message) = item.as_event().unwrap().content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "The _edited_ first message");

    // TODO: After adding raw timeline items, check for one here.

    // Test redactions work.
    timeline
        .handle_live_message_event(
            &ALICE,
            RoomMessageEventContent::text_plain("The second message"),
        )
        .await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let second_event_id = item.as_event().unwrap().event_id().unwrap();

    timeline.handle_live_redaction(&BOB, second_event_id).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 2, value } => value);
    assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::RedactedMessage);

    // TODO: After adding raw timeline items, check for one here.

    // Test reactions work.
    timeline
        .handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("The third message"))
        .await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let third_event_id = item.as_event().unwrap().event_id().unwrap();

    let rel = Annotation::new(third_event_id.to_owned(), "+1".to_owned());
    timeline.handle_live_message_event(&BOB, ReactionEventContent::new(rel)).await;
    timeline.handle_live_redaction(&BOB, second_event_id).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 3, value } => value);
    assert_eq!(item.as_event().unwrap().reactions().len(), 1);

    // TODO: After adding raw timeline items, check for one here.

    assert_eq!(timeline.inner.items().await.len(), 4);
}

#[async_test]
async fn test_filter_always_false() {
    let timeline = TestTimeline::new().with_settings(TimelineInnerSettings {
        event_filter: Arc::new(|_, _| false),
        ..Default::default()
    });

    timeline
        .handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("The first message"))
        .await;

    timeline
        .handle_live_redacted_message_event(&ALICE, RedactedRoomMessageEventContent::new())
        .await;

    timeline
        .handle_live_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            RoomMemberEventContent::new(MembershipState::Join),
            None,
        )
        .await;

    timeline
        .handle_live_state_event(&ALICE, RoomNameEventContent::new("Alice's room".to_owned()), None)
        .await;

    assert_eq!(timeline.inner.items().await.len(), 0);
}

#[async_test]
async fn test_custom_filter() {
    // Filter out all state events.
    let timeline = TestTimeline::new().with_settings(TimelineInnerSettings {
        event_filter: Arc::new(|ev, _| matches!(ev, AnySyncTimelineEvent::MessageLike(_))),
        ..Default::default()
    });
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("The first message"))
        .await;
    let _item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    timeline
        .handle_live_redacted_message_event(&ALICE, RedactedRoomMessageEventContent::new())
        .await;
    let _item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    timeline
        .handle_live_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            RoomMemberEventContent::new(MembershipState::Join),
            None,
        )
        .await;

    timeline
        .handle_live_state_event(&ALICE, RoomNameEventContent::new("Alice's room".to_owned()), None)
        .await;

    assert_eq!(timeline.inner.items().await.len(), 3);
}

#[async_test]
async fn test_hide_failed_to_parse() {
    let timeline = TestTimeline::new()
        .with_settings(TimelineInnerSettings { add_failed_to_parse: false, ..Default::default() });

    // m.room.message events must have a msgtype and body in content, so this
    // event with an empty content object should fail to deserialize.
    timeline
        .handle_live_custom_event(sync_timeline_event!({
            "content": {},
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 10,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        }))
        .await;

    // Similar to above, the m.room.member state event must also not have an
    // empty content object.
    timeline
        .handle_live_custom_event(sync_timeline_event!({
            "content": {},
            "event_id": "$d5G0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 2179,
            "sender": "@alice:example.org",
            "type": "m.room.member",
            "state_key": "@alice:example.org",
        }))
        .await;

    assert_eq!(timeline.inner.items().await.len(), 0);
}

#[async_test]
async fn test_event_type_filter_include_only_room_names() {
    // Only return room name events
    let event_filter = TimelineEventTypeFilter::Include(vec![TimelineEventType::RoomName]);

    let timeline = TestTimeline::new().with_settings(TimelineInnerSettings {
        event_filter: Arc::new(move |event, _| event_filter.filter(event)),
        ..Default::default()
    });

    // Add a non-encrypted message event
    timeline
        .handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("The first message"))
        .await;
    // Add a couple of room name events
    timeline
        .handle_live_state_event(
            &ALICE,
            RoomNameEventContent::new("A new room name".to_owned()),
            None,
        )
        .await;
    timeline
        .handle_live_state_event(
            &ALICE,
            RoomNameEventContent::new("A new room name (again)".to_owned()),
            None,
        )
        .await;
    // And a different state event
    timeline
        .handle_live_state_event(
            &ALICE,
            RoomTopicEventContent::new("A new room topic".to_owned()),
            None,
        )
        .await;

    // The timeline should contain only the room name events
    let event_items: Vec<Arc<TimelineItem>> = timeline.get_event_items().await;
    let text_message_items_count = event_items.iter().filter(is_text_message_item).count();
    let room_name_items_count = event_items.iter().filter(is_room_name_item).count();
    let room_topic_items_count = event_items.iter().filter(is_room_topic_item).count();
    assert_eq!(event_items.len(), 2);
    assert_eq!(text_message_items_count, 0);
    assert_eq!(room_name_items_count, 2);
    assert_eq!(room_topic_items_count, 0);
}

#[async_test]
async fn test_event_type_filter_exclude_messages() {
    // Don't return any messages
    let event_filter = TimelineEventTypeFilter::Exclude(vec![TimelineEventType::RoomMessage]);

    let timeline = TestTimeline::new().with_settings(TimelineInnerSettings {
        event_filter: Arc::new(move |event, _| event_filter.filter(event)),
        ..Default::default()
    });

    // Add a message event
    timeline
        .handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("The first message"))
        .await;
    // Add a couple of room name state events
    timeline
        .handle_live_state_event(
            &ALICE,
            RoomNameEventContent::new("A new room name".to_owned()),
            None,
        )
        .await;
    timeline
        .handle_live_state_event(
            &ALICE,
            RoomNameEventContent::new("A new room name (again)".to_owned()),
            None,
        )
        .await;
    // And a different state event
    timeline
        .handle_live_state_event(
            &ALICE,
            RoomTopicEventContent::new("A new room topic".to_owned()),
            None,
        )
        .await;

    // The timeline should contain everything except for the message event.
    let event_items: Vec<Arc<TimelineItem>> = timeline.get_event_items().await;
    let text_message_items_count = event_items.iter().filter(is_text_message_item).count();
    let room_name_items_count = event_items.iter().filter(is_room_name_item).count();
    let room_topic_items_count = event_items.iter().filter(is_room_topic_item).count();
    assert_eq!(event_items.len(), 3);
    assert_eq!(text_message_items_count, 0);
    assert_eq!(room_name_items_count, 2);
    assert_eq!(room_topic_items_count, 1);
}

impl TestTimeline {
    async fn get_event_items(&self) -> Vec<Arc<TimelineItem>> {
        self.inner
            .items()
            .await
            .into_iter()
            .filter(|i| matches!(i.kind, TimelineItemKind::Event(_)))
            .collect()
    }
}

fn is_text_message_item(item: &&Arc<TimelineItem>) -> bool {
    match item.kind() {
        TimelineItemKind::Event(event) => match &event.content {
            TimelineItemContent::Message(message) => {
                matches!(message.msgtype, MessageType::Text(_))
            }
            _ => false,
        },
        _ => false,
    }
}

fn is_room_name_item(item: &&Arc<TimelineItem>) -> bool {
    match item.kind() {
        TimelineItemKind::Event(event) => match &event.content {
            TimelineItemContent::OtherState(state) => {
                matches!(state.content, AnyOtherFullStateEventContent::RoomName(_))
            }
            _ => false,
        },
        _ => false,
    }
}

fn is_room_topic_item(item: &&Arc<TimelineItem>) -> bool {
    match item.kind() {
        TimelineItemKind::Event(event) => match &event.content {
            TimelineItemContent::OtherState(state) => {
                matches!(state.content, AnyOtherFullStateEventContent::RoomTopic(_))
            }
            _ => false,
        },
        _ => false,
    }
}
