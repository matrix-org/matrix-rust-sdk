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

use std::{sync::Arc, time::Duration};

use assert_matches2::{assert_let, assert_matches};
use eyeball_im::VectorDiff;
use futures_core::Stream;
use futures_util::{FutureExt as _, StreamExt as _};
use matrix_sdk::{deserialized_responses::SyncTimelineEvent, test_utils::events::EventFactory};
use matrix_sdk_test::{async_test, sync_timeline_event, ALICE, BOB};
use ruma::{
    event_id, events::AnyMessageLikeEventContent, server_name, uint, EventId,
    MilliSecondsSinceUnixEpoch, OwnedEventId,
};
use stream_assert::assert_next_matches;
use tokio::time::timeout;

use crate::timeline::{
    controller::TimelineEnd, event_item::RemoteEventOrigin, tests::TestTimeline, ReactionStatus,
    TimelineItem,
};

const REACTION_KEY: &str = "👍";

/// Assert that we receive an item update for an event at the given item index,
/// for the given event id.
///
/// A macro rather than a function to help lower compile times and get better
/// error locations.
macro_rules! assert_item_update {
    ($stream:expr, $event_id:expr, $index:expr) => {{
        // Expect an event timeline item update, with a short timeout.
        assert_let!(
            Ok(Some(VectorDiff::Set { index: i, value: event })) =
                timeout(Duration::from_secs(1), $stream.next()).await
        );

        // Expect at the right position.
        assert_eq!(i, $index);

        // Expect on the right event.
        let event_item = event.as_event().unwrap();
        assert_eq!(event_item.event_id().unwrap(), $event_id);

        event_item.clone()
    }};
}

/// Checks that the reaction to an event in a timeline item has accordingly
/// updated.
///
/// A macro rather than a function to help lower compile times and get better
/// error locations.
macro_rules! assert_reaction_is_updated {
    ($stream:expr, $event_id:expr, $index:expr, $is_remote_echo:literal) => {{
        let event = assert_item_update!($stream, $event_id, $index);
        let reactions = event.reactions().get(&REACTION_KEY.to_owned()).unwrap();
        let reaction = reactions.get(*ALICE).unwrap();
        match reaction.status {
            ReactionStatus::LocalToRemote(_) | ReactionStatus::LocalToLocal(_) => {
                assert!(!$is_remote_echo)
            }
            ReactionStatus::RemoteToRemote(_) => assert!($is_remote_echo),
        };
        event
    }};
}

#[async_test]
async fn test_add_reaction_on_non_existent_event() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline.toggle_reaction_local("nonexisting_unique_id", REACTION_KEY).await.unwrap_err();

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_add_reaction_success() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_uid, event_id, item_pos) = send_first_message(&timeline, &mut stream).await;

    // If I toggle a reaction on an event which didn't have any…
    timeline.toggle_reaction_local(&msg_uid, REACTION_KEY).await.unwrap();

    // The timeline item is updated, with a local echo for the reaction.
    assert_reaction_is_updated!(stream, &event_id, item_pos, false);

    // An event of the right kind is sent over to the server.
    {
        let sent_events = &timeline.data().sent_events.read().await;
        assert_eq!(sent_events.len(), 1);
        assert_matches!(&sent_events[0], AnyMessageLikeEventContent::Reaction(..));
    }

    // When the remote echo is received from sync,
    let f = EventFactory::new();
    timeline.handle_live_event(f.reaction(&event_id, REACTION_KEY.to_owned()).sender(*ALICE)).await;

    // The reaction is still present on the item, as a remote echo.
    assert_reaction_is_updated!(stream, &event_id, item_pos, true);

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_redact_reaction_success() {
    let timeline = TestTimeline::new();
    let f = &timeline.factory;

    let mut stream = timeline.subscribe().await;
    let (msg_uid, event_id, item_pos) = send_first_message(&timeline, &mut stream).await;

    // A reaction is added by sync.
    let reaction_id = event_id!("$reaction_id");
    timeline
        .handle_live_event(
            f.reaction(&event_id, REACTION_KEY.to_owned()).sender(&ALICE).event_id(reaction_id),
        )
        .await;
    assert_reaction_is_updated!(stream, &event_id, item_pos, true);

    // Toggling the reaction locally…
    timeline.toggle_reaction_local(&msg_uid, REACTION_KEY).await.unwrap();

    // Will immediately redact it on the item.
    let event = assert_item_update!(stream, &event_id, item_pos);
    assert!(event.reactions().get(&REACTION_KEY.to_owned()).is_none());
    // And send a redaction request for that reaction.
    {
        let redacted_events = &timeline.data().redacted.read().await;
        assert_eq!(redacted_events.len(), 1);
        assert_eq!(&redacted_events[0], reaction_id);
    }

    // When that redaction is confirmed by the server,
    timeline
        .handle_live_event(SyncTimelineEvent::new(sync_timeline_event!({
            "sender": *ALICE,
            "type": "m.room.redaction",
            "event_id": "$idb",
            "redacts": reaction_id,
            "origin_server_ts": 12344448,
            "content": {},
        })))
        .await;

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_reactions_store_timestamp() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let (msg_uid, event_id, msg_pos) = send_first_message(&timeline, &mut stream).await;

    // Creating a reaction adds a valid timestamp.
    let timestamp_before = MilliSecondsSinceUnixEpoch::now();

    timeline.toggle_reaction_local(&msg_uid, REACTION_KEY).await.unwrap();

    let event = assert_reaction_is_updated!(stream, &event_id, msg_pos, false);
    let reactions = event.reactions().get(&REACTION_KEY.to_owned()).unwrap();
    let timestamp = reactions.values().next().unwrap().timestamp;

    let now = MilliSecondsSinceUnixEpoch::now();
    assert!((timestamp_before..=now).contains(&timestamp),);
}

#[async_test]
async fn test_initial_reaction_timestamp_is_stored() {
    let timeline = TestTimeline::new();

    let f = EventFactory::new().sender(*ALICE);
    let message_event_id = EventId::new(server_name!("dummy.server"));
    let reaction_timestamp = MilliSecondsSinceUnixEpoch(uint!(39845));

    timeline
        .controller
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

    let items = timeline.controller.items().await;
    let reactions = items.last().unwrap().as_event().unwrap().reactions();
    let entry = reactions.get(&REACTION_KEY.to_owned()).unwrap();

    assert_eq!(entry.values().next().unwrap().timestamp, reaction_timestamp);
}

/// Returns the unique item id, the event id, and position of the message.
async fn send_first_message(
    timeline: &TestTimeline,
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
) -> (String, OwnedEventId, usize) {
    timeline.handle_live_event(timeline.factory.text_msg("I want you to react").sender(&BOB)).await;

    let item = assert_next_matches!(*stream, VectorDiff::PushBack { value } => value);
    let event_id = item.as_event().unwrap().as_remote().unwrap().event_id.clone();
    let position = timeline.len().await - 1;

    let day_divider = assert_next_matches!(*stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    (item.unique_id().to_owned(), event_id, position)
}
