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

use std::{ops::RangeInclusive, sync::Arc};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_core::Stream;
use matrix_sdk::test_utils::events::EventFactory;
use matrix_sdk_test::{async_test, ALICE, BOB};
use ruma::{
    event_id, events::relation::Annotation, server_name, uint, EventId, MilliSecondsSinceUnixEpoch,
    OwnedEventId, TransactionId, UserId,
};
use stream_assert::assert_next_matches;

use crate::timeline::{
    event_item::RemoteEventOrigin,
    inner::TimelineEnd,
    reactions::{ReactionAction, ReactionToggleResult},
    tests::{assert_event_is_updated, assert_no_more_updates, TestTimeline},
    TimelineEventItemId, TimelineItem,
};

const REACTION_KEY: &str = "👍";

#[async_test]
async fn test_add_reaction_failed() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;

    let reaction = create_reaction(&msg_id);
    let action = timeline.toggle_reaction_local(&reaction).await.unwrap();
    assert_let!(ReactionAction::SendRemote(txn_id) = action);
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, None, Some(&txn_id)).await;

    timeline
        .handle_reaction_response(&reaction, &ReactionToggleResult::AddFailure { txn_id })
        .await
        .unwrap_err();
    assert_reactions_are_removed(&mut stream, &msg_id, msg_pos).await;

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn test_add_reaction_on_non_existent_event() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let msg_id = EventId::new(server_name!("example.org")); // non existent event
    let reaction = create_reaction(&msg_id);

    timeline.toggle_reaction_local(&reaction).await.unwrap_err();

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn test_add_reaction_success() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;
    let reaction = create_reaction(&msg_id);

    let action = timeline.toggle_reaction_local(&reaction).await.unwrap();
    assert_let!(ReactionAction::SendRemote(txn_id) = action);
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, None, Some(&txn_id)).await;

    let event_id = EventId::new(server_name!("example.org"));
    timeline
        .handle_reaction_response(
            &reaction,
            &ReactionToggleResult::AddSuccess { event_id: event_id.clone(), txn_id },
        )
        .await
        .unwrap();
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, Some(&event_id), None).await;

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn test_redact_reaction_success() {
    let timeline = TestTimeline::new();
    let f = &timeline.factory;

    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;
    let reaction = create_reaction(&msg_id);

    let event_id = event_id!("$1");
    timeline
        .handle_live_event(
            f.reaction(&msg_id, REACTION_KEY.to_owned()).sender(&ALICE).event_id(event_id),
        )
        .await;
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, Some(event_id), None).await;

    let action = timeline.toggle_reaction_local(&reaction).await.unwrap();
    assert_matches!(action, ReactionAction::RedactRemote(_));
    assert_reactions_are_removed(&mut stream, &msg_id, msg_pos).await;

    timeline
        .handle_reaction_response(&reaction, &ReactionToggleResult::RedactSuccess)
        .await
        .unwrap();

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn test_redact_reaction_failure() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;

    let f = &timeline.factory;

    let event_id = event_id!("$1");
    timeline
        .handle_live_event(
            f.reaction(&msg_id, REACTION_KEY.to_owned()).sender(&ALICE).event_id(event_id),
        )
        .await;
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, Some(event_id), None).await;

    let reaction = create_reaction(&msg_id);
    let action = timeline.toggle_reaction_local(&reaction).await.unwrap();
    assert_matches!(action, ReactionAction::RedactRemote(_));
    assert_reactions_are_removed(&mut stream, &msg_id, msg_pos).await;

    timeline
        .handle_reaction_response(
            &reaction,
            &ReactionToggleResult::RedactFailure { event_id: event_id.to_owned() },
        )
        .await
        .unwrap_err();
    assert_reaction_is_updated(&mut stream, &msg_id, msg_pos, Some(event_id), None).await;

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn test_redact_reaction_from_non_existing_event() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let reaction_id = EventId::new(server_name!("example.org")); // non existent event

    timeline.handle_local_redaction_event(&reaction_id).await;

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn test_toggle_during_request_resolves_new_action() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;
    let reaction = create_reaction(&msg_id);

    // Add a reaction
    let action = timeline.toggle_reaction_local(&reaction).await.unwrap();
    assert_let!(ReactionAction::SendRemote(txn_id) = action);
    assert_reaction_is_added(&mut stream, &msg_id, msg_pos).await;

    // Toggle before response is received
    let action = timeline.toggle_reaction_local(&reaction).await.unwrap();
    assert_matches!(action, ReactionAction::None);
    assert_reactions_are_removed(&mut stream, &msg_id, msg_pos).await;

    // Receive response and resolve to a redaction action
    let event_id = EventId::new(server_name!("example.org")); // non existent event
    let action = timeline
        .handle_reaction_response(&reaction, &ReactionToggleResult::AddSuccess { txn_id, event_id })
        .await
        .unwrap();
    assert_matches!(action, ReactionAction::RedactRemote(event_id) => event_id);
    assert_no_more_updates(&mut stream).await;

    // Toggle before response is received
    let action = timeline.toggle_reaction_local(&reaction).await.unwrap();
    assert_matches!(action, ReactionAction::None);
    assert_reaction_is_added(&mut stream, &msg_id, msg_pos).await;

    // Receive response and resolve to a send action
    let action = timeline
        .handle_reaction_response(&reaction, &ReactionToggleResult::RedactSuccess)
        .await
        .unwrap();
    assert_let!(ReactionAction::SendRemote(txn_id) = action);
    assert_no_more_updates(&mut stream).await;

    // Receive response and resolve to no new action
    let event_id = EventId::new(server_name!("example.org")); // non existent event
    let action = timeline
        .handle_reaction_response(&reaction, &ReactionToggleResult::AddSuccess { txn_id, event_id })
        .await
        .unwrap();
    assert_matches!(action, ReactionAction::None);
    assert_reaction_is_added(&mut stream, &msg_id, msg_pos).await;

    assert_no_more_updates(&mut stream).await;
}

#[async_test]
async fn test_reactions_store_timestamp() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_id, msg_pos) = send_first_message(&timeline, &mut stream).await;
    let reaction = create_reaction(&msg_id);

    // Creating a reaction adds a valid timestamp.
    let timestamp_before = MilliSecondsSinceUnixEpoch::now();
    let _ = timeline.toggle_reaction_local(&reaction).await.unwrap();
    let event = assert_event_is_updated(&mut stream, &msg_id, msg_pos).await;
    let reactions = event.reactions().get(&REACTION_KEY.to_owned()).unwrap();
    let timestamp = reactions.values().next().unwrap().timestamp;
    assert!(timestamp_range_until_now_from(timestamp_before).contains(&timestamp));

    // Failing a redaction.
    let _ = timeline.toggle_reaction_local(&reaction).await.unwrap();
    let _ = assert_event_is_updated(&mut stream, &msg_id, msg_pos).await;
    timeline
        .handle_reaction_response(
            &reaction,
            &ReactionToggleResult::RedactFailure { event_id: msg_id.clone() },
        )
        .await
        .unwrap_err();

    // Restores an event with a valid timestamp.
    let event = assert_event_is_updated(&mut stream, &msg_id, msg_pos).await;
    let reactions = event.reactions().get(&REACTION_KEY.to_owned()).unwrap();
    let new_timestamp = reactions.values().next().unwrap().timestamp;
    assert!(timestamp_range_until_now_from(timestamp_before).contains(&new_timestamp));
}

#[async_test]
async fn test_initial_reaction_timestamp_is_stored() {
    let timeline = TestTimeline::new();

    let f = EventFactory::new().sender(*ALICE);
    let message_event_id = EventId::new(server_name!("dummy.server"));
    let reaction_timestamp = MilliSecondsSinceUnixEpoch(uint!(39845));

    timeline
        .inner
        .add_events_at(
            vec![
                // Reaction comes first.
                f.reaction(&message_event_id, REACTION_KEY.to_owned())
                    .server_ts(reaction_timestamp)
                    .into_sync(),
                // Event comes next.
                f.text_msg("A").event_id(&message_event_id).into_sync(),
            ],
            TimelineEnd::Back,
            RemoteEventOrigin::Sync,
        )
        .await;

    let items = timeline.inner.items().await;
    let reactions = items.last().unwrap().as_event().unwrap().reactions();
    let entry = reactions.get(&REACTION_KEY.to_owned()).unwrap();

    assert_eq!(reaction_timestamp, entry.values().next().unwrap().timestamp);
}

fn create_reaction(related_message_id: &EventId) -> Annotation {
    let reaction_key = REACTION_KEY.to_owned();
    let msg_id = related_message_id.to_owned();
    Annotation::new(msg_id, reaction_key)
}

/// Returns the event id and position of the message.
async fn send_first_message(
    timeline: &TestTimeline,
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
) -> (OwnedEventId, usize) {
    timeline.handle_live_event(timeline.factory.text_msg("I want you to react").sender(&BOB)).await;

    let item = assert_next_matches!(*stream, VectorDiff::PushBack { value } => value);
    let event_id = item.as_event().unwrap().clone().event_id().unwrap().to_owned();
    let position = timeline.len().await - 1;

    let day_divider = assert_next_matches!(*stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    (event_id, position)
}

async fn assert_reaction_is_updated(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    related_to: &EventId,
    message_position: usize,
    expected_event_id: Option<&EventId>,
    expected_txn_id: Option<&TransactionId>,
) {
    let own_user_id: &UserId = &ALICE;
    let event = assert_event_is_updated(stream, related_to, message_position).await;
    let (reaction_tx_id, reaction_event_id) = {
        let reactions = event.reactions().get(&REACTION_KEY.to_owned()).unwrap();
        let reaction = reactions.get(own_user_id).unwrap();
        match &reaction.id {
            TimelineEventItemId::TransactionId(txn_id) => (Some(txn_id), None),
            TimelineEventItemId::EventId(event_id) => (None, Some(event_id)),
        }
    };
    assert_eq!(reaction_tx_id, expected_txn_id.map(|it| it.to_owned()).as_ref());
    assert_eq!(reaction_event_id, expected_event_id.map(|it| it.to_owned()).as_ref());
}

async fn assert_reaction_is_added(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    related_to: &EventId,
    message_position: usize,
) {
    let own_user_id: &UserId = &ALICE;
    let event = assert_event_is_updated(stream, related_to, message_position).await;
    let reactions = event.reactions().get(&REACTION_KEY.to_owned()).unwrap();
    assert!(reactions.get(own_user_id).is_some());
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

fn timestamp_range_until_now_from(
    timestamp: MilliSecondsSinceUnixEpoch,
) -> RangeInclusive<MilliSecondsSinceUnixEpoch> {
    timestamp..=MilliSecondsSinceUnixEpoch::now()
}
