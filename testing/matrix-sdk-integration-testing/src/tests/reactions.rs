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

use anyhow::Result;
use assert_matches::assert_matches;
use assert_matches2::assert_let;
use assign::assign;
use eyeball_im::{Vector, VectorDiff};
use futures_core::Stream;
use futures_util::{future::join_all, FutureExt, StreamExt};
use matrix_sdk::ruma::{
    api::client::room::create_room::v3::Request as CreateRoomRequest,
    events::{relation::Annotation, room::message::RoomMessageEventContent},
    EventId, MilliSecondsSinceUnixEpoch, UserId,
};
use matrix_sdk_ui::timeline::{EventTimelineItem, RoomExt, TimelineItem};
use tokio::{
    spawn,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{debug, warn};

use crate::helpers::TestClientBuilder;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_toggling_reaction() -> Result<()> {
    // Set up sync for user Alice, and create a room.
    let alice = TestClientBuilder::new("alice".to_owned())
        .randomize_username()
        .use_sqlite()
        .build()
        .await?;

    let alice_clone = alice.clone();
    let alice_sync = spawn(async move {
        alice_clone.sync(Default::default()).await.expect("sync failed!");
    });

    debug!("Creating room…");
    let user_id = alice.user_id().unwrap().to_owned();
    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
        }))
        .await?;

    // Send a first message and then wait for the remote echo.
    //
    // First, spawn the task waiting for the remote echo, before sending the
    // message. Otherwise, we might get the remote echo before we started
    // waiting for it.

    let timeline = room.timeline().await.unwrap();
    let (mut items, mut stream) = timeline.subscribe().await;

    let event_id_task: JoinHandle<Result<_>> = spawn(async move {
        let find_event_id = |items: &Vector<Arc<TimelineItem>>| {
            items.iter().find_map(|item| {
                let event = item.as_event()?;
                if event.content().as_message()?.body().trim() == "hi!" {
                    event.event_id().map(|event_id| event_id.to_owned())
                } else {
                    None
                }
            })
        };

        if let Some(event_id) = find_event_id(&items) {
            return Ok(event_id);
        }

        warn!(?items, "Waiting for updates…");

        while let Some(diff) = stream.next().await {
            warn!(?diff, "received a diff");
            diff.apply(&mut items);
            if let Some(event_id) = find_event_id(&items) {
                return Ok(event_id);
            }
        }

        unreachable!();
    });

    // Create a timeline for this room.
    debug!("Creating timeline…");
    let timeline = room.timeline().await.unwrap();

    // Send message.
    debug!("Sending initial message…");
    timeline.send(RoomMessageEventContent::text_plain("hi!").into()).await;

    let event_id = timeout(Duration::from_secs(10), event_id_task)
        .await
        .expect("timeout")
        .expect("failed to join tokio task")
        .expect("waiting for the event id failed");

    alice_sync.abort();
    let _ = alice_sync.await;

    // Give a bit of time for the timeline to process all sync updates.
    sleep(Duration::from_secs(1)).await;

    let (mut items, mut stream) = timeline.subscribe().await;

    // Skip all stream updates that have happened so far.
    debug!("Skipping all other stream updates…");
    while let Some(Some(diff)) = stream.next().now_or_never() {
        diff.apply(&mut items);
    }

    let message_position = items
        .iter()
        .enumerate()
        .find_map(|(i, item)| (item.as_event()?.event_id()? == event_id).then_some(i))
        .expect("couldn't find the final position for the event id");

    let reaction_key = "👍";
    let reaction = Annotation::new(event_id.clone(), reaction_key.into());

    // Toggle reaction multiple times.
    let all_tests = async move {
        for _ in 0..3 {
            debug!("Starting the toggle reaction tests…");

            // Add
            timeline.toggle_reaction(&reaction).await.expect("toggling reaction");
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_remote_added(&mut stream, &user_id, &event_id, &reaction, message_position)
                .await;

            // Redact
            timeline.toggle_reaction(&reaction).await.expect("toggling reaction the second time");
            assert_redacted(&mut stream, &event_id, message_position).await;

            // Add, redact, add, redact, add
            join_all((0..5).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_redacted(&mut stream, &event_id, message_position).await;
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_redacted(&mut stream, &event_id, message_position).await;
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_remote_added(&mut stream, &user_id, &event_id, &reaction, message_position)
                .await;

            // Redact, add, redact, add
            join_all((0..4).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;
            assert_redacted(&mut stream, &event_id, message_position).await;
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_redacted(&mut stream, &event_id, message_position).await;
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_remote_added(&mut stream, &user_id, &event_id, &reaction, message_position)
                .await;

            // Redact, add, redact, add, redact
            join_all((0..5).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;
            assert_redacted(&mut stream, &event_id, message_position).await;
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_redacted(&mut stream, &event_id, message_position).await;
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_redacted(&mut stream, &event_id, message_position).await;

            // Add, redact, add, redact
            join_all((0..4).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_redacted(&mut stream, &event_id, message_position).await;
            assert_local_added(&mut stream, &user_id, &event_id, &reaction, message_position).await;
            assert_redacted(&mut stream, &event_id, message_position).await;
        }
    };

    timeout(Duration::from_secs(10), all_tests).await.expect("timed out");

    Ok(())
}

async fn assert_local_added(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    user_id: &UserId,
    event_id: &EventId,
    reaction: &Annotation,
    message_position: usize,
) {
    let event = assert_event_is_updated(stream, event_id, message_position).await;

    let (reaction_tx_id, reaction_event_id) = {
        let reactions = event.reactions().get(&reaction.key).unwrap();
        let reaction = reactions.by_sender(user_id).next().unwrap();
        reaction.to_owned()
    };
    assert_matches!(reaction_tx_id, Some(_));
    // Event ID hasn't been received from homeserver yet
    assert_matches!(reaction_event_id, None);
}

async fn assert_redacted(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    event_id: &EventId,
    message_position: usize,
) {
    let event = assert_event_is_updated(stream, event_id, message_position).await;
    assert!(event.reactions().is_empty());
}

async fn assert_remote_added(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    user_id: &UserId,
    event_id: &EventId,
    reaction: &Annotation,
    message_position: usize,
) {
    let event = assert_event_is_updated(stream, event_id, message_position).await;

    let reactions = event.reactions().get(&reaction.key).unwrap();
    assert_eq!(reactions.senders().count(), 1);

    let reaction = reactions.by_sender(user_id).next().unwrap();
    let (reaction_tx_id, reaction_event_id) = reaction;
    assert_matches!(reaction_tx_id, None);
    assert_matches!(reaction_event_id, Some(value) => value);

    // Remote event should have a timestamp <= than now.
    // Note: this can actually be equal because if the timestamp from
    // server is not available, it might be created with a local call to `now()`
    let reaction = reactions.senders().next();
    assert!(reaction.unwrap().timestamp <= MilliSecondsSinceUnixEpoch::now());
}

async fn assert_event_is_updated(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    event_id: &EventId,
    index: usize,
) -> EventTimelineItem {
    assert_let!(Some(VectorDiff::Set { index: i, value: event }) = stream.next().await);
    assert_eq!(i, index, "unexpected position for event update, value = {event:?}");

    let event = event.as_event().unwrap();
    assert_eq!(event.event_id().unwrap(), event_id);

    event.to_owned()
}
