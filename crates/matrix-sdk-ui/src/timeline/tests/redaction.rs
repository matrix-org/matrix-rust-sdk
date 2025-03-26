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
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use imbl::vector;
use matrix_sdk_test::{async_test, ALICE, BOB};
use ruma::events::{
    reaction::RedactedReactionEventContent, room::message::OriginalSyncRoomMessageEvent,
    FullStateEventContent,
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::{
    event_item::RemoteEventOrigin, AggregatedTimelineItemContent,
    AggregatedTimelineItemContentKind, AnyOtherFullStateEventContent, TimelineDetails,
    TimelineItemContent,
};

#[async_test]
async fn test_redact_state_event() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    timeline.handle_live_event(f.room_name("Fancy room name").sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::OtherState(state) = item.content());
    assert_matches!(
        state.content,
        AnyOtherFullStateEventContent::RoomName(FullStateEventContent::Original { .. })
    );

    timeline.handle_live_event(f.redaction(item.event_id().unwrap()).sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_let!(TimelineItemContent::OtherState(state) = item.content());
    assert_matches!(
        state.content,
        AnyOtherFullStateEventContent::RoomName(FullStateEventContent::Redacted(_))
    );
}

#[async_test]
async fn test_redact_replied_to_event() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    timeline.handle_live_event(f.text_msg("Hello, world!").sender(&ALICE)).await;

    let first_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_matches!(
        first_item.content(),
        TimelineItemContent::Aggregated(AggregatedTimelineItemContent {
            kind: AggregatedTimelineItemContentKind::Message(_),
            ..
        })
    );
    let first_event: OriginalSyncRoomMessageEvent =
        first_item.original_json().unwrap().deserialize_as().unwrap();

    timeline
        .handle_live_event(f.text_msg("Hello, alice.").sender(&BOB).reply_to(&first_event.event_id))
        .await;

    let second_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let aggregated = second_item.content().as_aggregated().unwrap();
    let in_reply_to = aggregated.in_reply_to.clone().unwrap();
    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_matches!(
        replied_to_event.content(),
        TimelineItemContent::Aggregated(AggregatedTimelineItemContent {
            kind: AggregatedTimelineItemContentKind::Message(_),
            ..
        })
    );

    timeline.handle_live_event(f.redaction(first_item.event_id().unwrap()).sender(&ALICE)).await;

    let second_item_again =
        assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let aggregated = second_item_again.content().as_aggregated().unwrap();
    let in_reply_to = aggregated.in_reply_to.clone().unwrap();
    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_matches!(replied_to_event.content(), TimelineItemContent::RedactedMessage);

    let first_item_again =
        assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_matches!(first_item_again.content(), TimelineItemContent::RedactedMessage);
    assert_matches!(first_item_again.original_json(), None);
}

#[async_test]
async fn test_reaction_redaction() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    timeline.handle_live_event(f.text_msg("hi!").sender(&ALICE)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.content().reactions().len(), 0);

    let msg_event_id = item.event_id().unwrap();

    timeline.handle_live_event(f.reaction(msg_event_id, "+1").sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.content().reactions().len(), 1);

    // TODO: After adding raw timeline items, check for one here

    let reaction_event_id = item.event_id().unwrap();

    timeline.handle_live_event(f.redaction(reaction_event_id).sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.content().reactions().len(), 0);
}

#[async_test]
async fn test_reaction_redaction_timeline_filter() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    // Initialise a timeline with a redacted reaction.
    timeline
        .controller
        .handle_remote_events_with_diffs(
            vec![VectorDiff::Append {
                values: vector![f
                    .redacted(*ALICE, RedactedReactionEventContent::new())
                    .into_event()],
            }],
            RemoteEventOrigin::Sync,
        )
        .await;
    // Timeline items are actually empty.
    assert_eq!(timeline.controller.items().await.len(), 0);

    // Adding a live redacted reaction does nothing.
    timeline.handle_live_event(f.redacted(&ALICE, RedactedReactionEventContent::new())).await;
    assert_eq!(timeline.controller.items().await.len(), 0);

    // Adding a room message
    timeline.handle_live_event(f.text_msg("hi!").sender(&ALICE)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    // Creates a date divider and the message.
    assert_eq!(timeline.controller.items().await.len(), 2);

    // Reaction is attached to the message and doesn't add a timeline item.
    timeline.handle_live_event(f.reaction(item.event_id().unwrap(), "+1").sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let reaction_event_id = item.event_id().unwrap();
    assert_eq!(timeline.controller.items().await.len(), 2);

    // Redacting the reaction doesn't add a timeline item.
    timeline.handle_live_event(f.redaction(reaction_event_id).sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.content().reactions().len(), 0);
    assert_eq!(timeline.controller.items().await.len(), 2);
}
