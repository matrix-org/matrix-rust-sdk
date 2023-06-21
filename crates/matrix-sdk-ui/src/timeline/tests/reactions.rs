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

use std::sync::Arc;

use eyeball_im::VectorDiff;
use futures_core::Stream;
use matrix_sdk_test::async_test;
use ruma::{
    events::{
        reaction::ReactionEventContent, relation::Annotation,
        room::message::RoomMessageEventContent, AnyMessageLikeEventContent,
    },
    server_name, EventId, OwnedEventId, TransactionId,
};
use stream_assert::assert_next_matches;

use crate::timeline::{
    reactions::ReactionToggleResult,
    tests::{
        helpers::assert_event_is_updated, helpers::assert_no_more_updates, TestTimeline, ALICE, BOB,
    },
    TimelineItem,
};

const REACTION_KEY: &str = "👍";

#[async_test]
async fn add_reaction_failed() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;
    let reaction = create_reaction(&msg_id);
    let event_content = create_reaction_event_content(&reaction);

    let txn_id = timeline.handle_local_event(event_content.clone()).await;
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, None, Some(&txn_id)).await;

    timeline
        .update_reaction_send_state(&reaction, &ReactionToggleResult::add_failure(&txn_id))
        .await;
    assert_reactions_are_removed(&mut stream, &msg_id, msg_pos).await;

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn add_reaction_success() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;
    let reaction = create_reaction(&msg_id);
    let event_content = create_reaction_event_content(&reaction);

    let txn_id = timeline.handle_local_event(event_content.clone()).await;
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, None, Some(&txn_id)).await;

    let event_id = EventId::new(server_name!("example.org"));
    timeline
        .update_reaction_send_state(
            &reaction,
            &ReactionToggleResult::add_success(&event_id, &txn_id),
        )
        .await;
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, Some(&event_id), None).await;

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn redact_reaction_success() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;
    let reaction = create_reaction(&msg_id);

    let event_id = timeline.handle_live_reaction(&ALICE, &reaction).await;
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, Some(&event_id), None).await;

    timeline.handle_local_redaction_event(&event_id, Default::default()).await;
    assert_reactions_are_removed(&mut stream, &msg_id, msg_pos).await;

    timeline.update_reaction_send_state(&reaction, &ReactionToggleResult::redact_success()).await;

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn redact_reaction_failure() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;
    let reaction = create_reaction(&msg_id);

    let event_id = timeline.handle_live_reaction(&ALICE, &reaction).await;
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, Some(&event_id), None).await;

    timeline.handle_local_redaction_event(&event_id, Default::default()).await;
    assert_reactions_are_removed(&mut stream, &msg_id, msg_pos).await;

    timeline
        .update_reaction_send_state(&reaction, &ReactionToggleResult::redact_failure(&event_id))
        .await;
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, Some(&event_id), None).await;

    assert_no_more_updates(&mut stream).await;
}

fn create_reaction(related_message_id: &EventId) -> Annotation {
    let reaction_key = REACTION_KEY.to_owned();
    let msg_id = related_message_id.to_owned();
    Annotation::new(msg_id, reaction_key)
}

fn create_reaction_event_content(annotation: &Annotation) -> AnyMessageLikeEventContent {
    AnyMessageLikeEventContent::Reaction(ReactionEventContent::from(annotation.clone()))
}

/// Returns the event id and position of the message.
async fn send_first_message(
    timeline: &TestTimeline,
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
) -> (OwnedEventId, usize) {
    timeline
        .handle_live_message_event(&BOB, RoomMessageEventContent::text_plain("I want you to react"))
        .await;

    let _day_divider = assert_next_matches!(*stream, VectorDiff::PushBack { value } => value);

    let item = assert_next_matches!(*stream, VectorDiff::PushBack { value } => value);
    let event_id = item.as_event().unwrap().clone().event_id().unwrap().to_owned();
    let position = timeline.len().await - 1;
    (event_id, position)
}

async fn assert_reaction_is_updated(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    related_to: &EventId,
    message_position: usize,
    expected_event_id: Option<&EventId>,
    expected_txn_id: Option<&TransactionId>,
) {
    let own_user_id = &ALICE;
    let event = assert_event_is_updated(stream, related_to, message_position).await;
    let (reaction_tx_id, reaction_event_id) = {
        let reactions = event.reactions().get(&REACTION_KEY.to_owned()).unwrap();
        let reaction = reactions.by_sender(own_user_id).next().unwrap();
        reaction.to_owned()
    };
    assert_eq!(reaction_tx_id, expected_txn_id.map(|it| it.to_owned()).as_ref());
    assert_eq!(reaction_event_id, expected_event_id.map(|it| it.to_owned()).as_ref());
}

async fn assert_reactions_are_removed(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    related_to: &EventId,
    message_position: usize,
) {
    let event = assert_event_is_updated(stream, related_to, message_position).await;
    let reactions = event.reactions().get(&REACTION_KEY.to_owned());
    assert!(reactions.is_none());
}
