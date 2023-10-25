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
        },
        AnySyncTimelineEvent,
    },
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::{inner::TimelineInnerSettings, TimelineItemContent};

#[async_test]
async fn default_filter() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // Test edits work.
    timeline
        .handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("The first message"))
        .await;
    let _day_divider = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
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
async fn filter_always_false() {
    let timeline = TestTimeline::new().with_settings(TimelineInnerSettings {
        event_filter: Arc::new(|_| false),
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
async fn custom_filter() {
    // Filter out all state events.
    let timeline = TestTimeline::new().with_settings(TimelineInnerSettings {
        event_filter: Arc::new(|ev| matches!(ev, AnySyncTimelineEvent::MessageLike(_))),
        ..Default::default()
    });
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("The first message"))
        .await;
    let _day_divider = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

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
async fn hide_failed_to_parse() {
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
