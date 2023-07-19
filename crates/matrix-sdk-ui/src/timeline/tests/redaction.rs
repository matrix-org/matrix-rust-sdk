// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use imbl::vector;
use matrix_sdk_test::async_test;
use ruma::{
    events::{
        reaction::{ReactionEventContent, RedactedReactionEventContent},
        relation::Annotation,
        room::{
            message::{
                ForwardThread, OriginalSyncRoomMessageEvent, RedactedRoomMessageEventContent,
                RoomMessageEventContent,
            },
            name::RoomNameEventContent,
        },
        FullStateEventContent,
    },
    owned_room_id,
};
use serde_json::json;
use stream_assert::assert_next_matches;

use super::{sync_timeline_event, TestTimeline, ALICE, BOB};
use crate::timeline::{AnyOtherFullStateEventContent, TimelineDetails, TimelineItemContent};

#[async_test]
async fn redact_state_event() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    timeline
        .handle_live_state_event(
            &ALICE,
            RoomNameEventContent::new(Some("Fancy room name".to_owned())),
            None,
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let state = assert_matches!(item.content(), TimelineItemContent::OtherState(st) => st);
    assert_matches!(
        state.content,
        AnyOtherFullStateEventContent::RoomName(FullStateEventContent::Original { .. })
    );

    timeline.handle_live_redaction(&ALICE, item.event_id().unwrap()).await;

    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let state = assert_matches!(item.content(), TimelineItemContent::OtherState(st) => st);
    assert_matches!(
        state.content,
        AnyOtherFullStateEventContent::RoomName(FullStateEventContent::Redacted(_))
    );
}

#[async_test]
async fn redact_replied_to_event() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    timeline
        .handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("Hello, world!"))
        .await;

    let first_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_matches!(first_item.content(), TimelineItemContent::Message(_));
    let first_event: OriginalSyncRoomMessageEvent =
        first_item.original_json().unwrap().deserialize_as().unwrap();

    timeline
        .handle_live_message_event(
            &BOB,
            RoomMessageEventContent::text_plain("Hello, alice.").make_reply_to(
                &first_event.into_full_event(owned_room_id!("!whocares:local.host")),
                ForwardThread::Yes,
            ),
        )
        .await;

    let second_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let message = second_item.content().as_message().unwrap();
    let in_reply_to = message.in_reply_to().unwrap();
    let replied_to_event = assert_matches!(&in_reply_to.event, TimelineDetails::Ready(val) => val);
    assert_matches!(replied_to_event.content(), TimelineItemContent::Message(_));

    timeline.handle_live_redaction(&ALICE, first_item.event_id().unwrap()).await;

    let first_item_again =
        assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_matches!(first_item_again.content(), TimelineItemContent::RedactedMessage);

    let second_item_again =
        assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let message = second_item_again.content().as_message().unwrap();
    let in_reply_to = message.in_reply_to().unwrap();
    let replied_to_event = assert_matches!(&in_reply_to.event, TimelineDetails::Ready(val) => val);
    assert_matches!(replied_to_event.content(), TimelineItemContent::RedactedMessage);
}

#[async_test]
async fn reaction_redaction() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("hi!")).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.reactions().len(), 0);

    let msg_event_id = item.event_id().unwrap();

    let rel = Annotation::new(msg_event_id.to_owned(), "+1".to_owned());
    timeline.handle_live_message_event(&BOB, ReactionEventContent::new(rel)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.reactions().len(), 1);

    // TODO: After adding raw timeline items, check for one here

    let reaction_event_id = item.event_id().unwrap();

    timeline.handle_live_redaction(&BOB, reaction_event_id).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.reactions().len(), 0);
}

#[async_test]
async fn reaction_redaction_timeline_filter() {
    let mut timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    // Initialise a timeline with a redacted reaction.
    timeline
        .inner
        .add_initial_events(vector![sync_timeline_event(
            timeline.make_redacted_message_event(*ALICE, RedactedReactionEventContent::new())
        )])
        .await;
    // Timeline items are actually empty.
    assert_eq!(timeline.inner.items().await.len(), 0);

    // Adding a live redacted reaction does nothing.
    timeline.handle_live_redacted_message_event(&ALICE, RedactedReactionEventContent::new()).await;
    assert_eq!(timeline.inner.items().await.len(), 0);

    // Adding a room message
    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("hi!")).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    // Creates a day divider and the message.
    assert_eq!(timeline.inner.items().await.len(), 2);

    // Reaction is attached to the message and doesn't add a timeline item.
    let rel = Annotation::new(item.event_id().unwrap().to_owned(), "+1".to_owned());
    timeline.handle_live_message_event(&BOB, ReactionEventContent::new(rel)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let reaction_event_id = item.event_id().unwrap();
    assert_eq!(timeline.inner.items().await.len(), 2);

    // Redacting the reaction doesn't add a timeline item.
    timeline.handle_live_redaction(&BOB, reaction_event_id).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.reactions().len(), 0);
    assert_eq!(timeline.inner.items().await.len(), 2);
}

#[async_test]
async fn receive_unredacted() {
    let timeline = TestTimeline::new();

    // send two events, second one redacted
    timeline
        .handle_live_message_event(
            &ALICE,
            RoomMessageEventContent::text_plain("about to be redacted"),
        )
        .await;
    timeline
        .handle_live_redacted_message_event(&ALICE, RedactedRoomMessageEventContent::new())
        .await;

    // redact the first one as well
    let items = timeline.inner.items().await;
    assert!(items[0].is_day_divider());
    let fst = items[1].as_event().unwrap();
    timeline.handle_live_redaction(&ALICE, fst.event_id().unwrap()).await;

    let items = timeline.inner.items().await;
    assert_eq!(items.len(), 3);
    let fst = items[1].as_event().unwrap();
    let snd = items[2].as_event().unwrap();

    // make sure we have two redacted events
    assert!(fst.content.is_redacted());
    assert!(snd.content.is_redacted());

    // send new events with the same event ID as the previous ones
    timeline
        .handle_live_custom_event(json!({
            "content": {
                "body": "unredacted #1",
                "msgtype": "m.text",
            },
            "sender": &*ALICE,
            "event_id": fst.event_id().unwrap(),
            "origin_server_ts": fst.timestamp(),
            "type": "m.room.message",
        }))
        .await;
    timeline
        .handle_live_custom_event(json!({
            "content": {
                "body": "unredacted #2",
                "msgtype": "m.text",
            },
            "sender": &*ALICE,
            "event_id": snd.event_id().unwrap(),
            "origin_server_ts": snd.timestamp(),
            "type": "m.room.message",
        }))
        .await;

    // make sure we still have two redacted events
    let items = timeline.inner.items().await;
    assert_eq!(items.len(), 3);
    assert!(items[1].as_event().unwrap().content.is_redacted());
    assert!(items[2].as_event().unwrap().content.is_redacted());
}
