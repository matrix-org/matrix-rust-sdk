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
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use matrix_sdk_test::{async_test, ALICE, BOB};
use ruma::events::{
    reaction::RedactedReactionEventContent,
    room::{
        message::{OriginalSyncRoomMessageEvent, RedactedRoomMessageEventContent},
        name::RoomNameEventContent,
    },
    FullStateEventContent,
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::{
    event_item::RemoteEventOrigin, inner::TimelineEnd, AnyOtherFullStateEventContent,
    TimelineDetails, TimelineItemContent,
};

#[async_test]
async fn test_redact_state_event() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    timeline
        .handle_live_state_event(
            &ALICE,
            RoomNameEventContent::new("Fancy room name".to_owned()),
            None,
        )
        .await;

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
    assert_matches!(first_item.content(), TimelineItemContent::Message(_));
    let first_event: OriginalSyncRoomMessageEvent =
        first_item.original_json().unwrap().deserialize_as().unwrap();

    timeline
        .handle_live_event(f.text_msg("Hello, alice.").sender(&BOB).reply_to(&first_event.event_id))
        .await;

    let second_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let message = second_item.content().as_message().unwrap();
    let in_reply_to = message.in_reply_to().unwrap();
    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_matches!(replied_to_event.content(), TimelineItemContent::Message(_));

    timeline.handle_live_event(f.redaction(first_item.event_id().unwrap()).sender(&ALICE)).await;

    let first_item_again =
        assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_matches!(first_item_again.content(), TimelineItemContent::RedactedMessage);
    assert_matches!(first_item_again.original_json(), None);

    let second_item_again =
        assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let message = second_item_again.content().as_message().unwrap();
    let in_reply_to = message.in_reply_to().unwrap();
    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_matches!(replied_to_event.content(), TimelineItemContent::RedactedMessage);
}

#[async_test]
async fn test_reaction_redaction() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    timeline.handle_live_event(f.text_msg("hi!").sender(&ALICE)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.reactions().len(), 0);

    let msg_event_id = item.event_id().unwrap();

    timeline.handle_live_event(f.reaction(msg_event_id, "+1".to_owned()).sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.reactions().len(), 1);

    // TODO: After adding raw timeline items, check for one here

    let reaction_event_id = item.event_id().unwrap();

    timeline.handle_live_event(f.redaction(reaction_event_id).sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.reactions().len(), 0);
}

#[async_test]
async fn test_reaction_redaction_timeline_filter() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    // Initialise a timeline with a redacted reaction.
    timeline
        .inner
        .add_events_at(
            vec![SyncTimelineEvent::new(
                timeline
                    .event_builder
                    .make_sync_redacted_message_event(*ALICE, RedactedReactionEventContent::new()),
            )],
            TimelineEnd::Back,
            RemoteEventOrigin::Sync,
        )
        .await;
    // Timeline items are actually empty.
    assert_eq!(timeline.inner.items().await.len(), 0);

    // Adding a live redacted reaction does nothing.
    timeline.handle_live_redacted_message_event(&ALICE, RedactedReactionEventContent::new()).await;
    assert_eq!(timeline.inner.items().await.len(), 0);

    // Adding a room message
    timeline.handle_live_event(f.text_msg("hi!").sender(&ALICE)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    // Creates a day divider and the message.
    assert_eq!(timeline.inner.items().await.len(), 2);

    // Reaction is attached to the message and doesn't add a timeline item.
    timeline
        .handle_live_event(f.reaction(item.event_id().unwrap(), "+1".to_owned()).sender(&BOB))
        .await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    let reaction_event_id = item.event_id().unwrap();
    assert_eq!(timeline.inner.items().await.len(), 2);

    // Redacting the reaction doesn't add a timeline item.
    timeline.handle_live_event(f.redaction(reaction_event_id).sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::Set { index: 0, value } => value);
    assert_eq!(item.reactions().len(), 0);
    assert_eq!(timeline.inner.items().await.len(), 2);
}

#[async_test]
async fn test_receive_unredacted() {
    let timeline = TestTimeline::new();

    let f = &timeline.factory;

    // send two events, second one redacted
    timeline.handle_live_event(f.text_msg("about to be redacted").sender(&ALICE)).await;
    timeline
        .handle_live_redacted_message_event(&ALICE, RedactedRoomMessageEventContent::new())
        .await;

    // redact the first one as well
    let items = timeline.inner.items().await;
    assert!(items[0].is_day_divider());
    let fst = items[1].as_event().unwrap();
    timeline.handle_live_event(f.redaction(fst.event_id().unwrap()).sender(&ALICE)).await;

    let items = timeline.inner.items().await;
    assert_eq!(items.len(), 3);
    let fst = items[1].as_event().unwrap();
    let snd = items[2].as_event().unwrap();

    // make sure we have two redacted events
    assert!(fst.content.is_redacted());
    assert!(snd.content.is_redacted());

    // send new events with the same event ID as the previous ones
    timeline
        .handle_live_event(
            f.text_msg("unredacted #1")
                .sender(*ALICE)
                .event_id(fst.event_id().unwrap())
                .server_ts(fst.timestamp()),
        )
        .await;

    timeline
        .handle_live_event(
            f.text_msg("unredacted #2")
                .sender(*ALICE)
                .event_id(snd.event_id().unwrap())
                .server_ts(snd.timestamp()),
        )
        .await;

    // make sure we still have two redacted events
    let items = timeline.inner.items().await;
    assert_eq!(items.len(), 3);
    assert!(items[1].as_event().unwrap().content.is_redacted());
    assert!(items[2].as_event().unwrap().content.is_redacted());
}
