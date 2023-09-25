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

use anyhow::{Ok, Result};
use assert_matches::assert_matches;
use assign::assign;
use eyeball_im::VectorDiff;
use futures_core::Stream;
use futures_util::{future::join_all, FutureExt, StreamExt};
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::{relation::Annotation, room::message::RoomMessageEventContent},
        EventId, MilliSecondsSinceUnixEpoch, RoomId, UserId,
    },
    Client, LoopCtrl,
};
use matrix_sdk_ui::timeline::{EventTimelineItem, RoomExt, TimelineItem};

use crate::helpers::get_client_for_user;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_toggling_reaction() -> Result<()> {
    // Set up
    let alice = get_client_for_user("alice".to_owned(), true).await?;
    let user_id = alice.user_id().unwrap();
    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
        }))
        .await?;
    let room_id = room.room_id();
    let timeline = room.timeline().await;
    let reaction_key = "👍";
    let (_items, mut stream) = timeline.subscribe().await;

    // Send message
    timeline.send(RoomMessageEventContent::text_plain("hi!").into()).await;

    // Sync until the remote echo arrives
    let event_id = loop {
        sync_room(&alice, room_id).await?;
        let items = timeline.items().await;
        let last_item = items.last().unwrap().as_event().unwrap();
        if !last_item.is_local_echo() {
            break last_item.event_id().unwrap().to_owned();
        }
    };

    // Skip all stream updates that have happened so far
    while stream.next().now_or_never().is_some() {}

    let message_position = timeline.items().await.len() - 1;
    let reaction = Annotation::new(event_id.clone(), reaction_key.into());

    // Toggle reaction multiple times
    for _ in 0..3 {
        // Add
        timeline.toggle_reaction(&reaction).await?;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_remote_added(&mut stream, user_id, &event_id, &reaction, message_position).await;

        // Redact
        timeline.toggle_reaction(&reaction).await?;
        assert_redacted(&mut stream, &event_id, message_position).await;

        // Add, redact, add, redact, add
        join_all((0..5).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_redacted(&mut stream, &event_id, message_position).await;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_redacted(&mut stream, &event_id, message_position).await;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_remote_added(&mut stream, user_id, &event_id, &reaction, message_position).await;

        // Redact, add, redact, add
        join_all((0..4).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;
        assert_redacted(&mut stream, &event_id, message_position).await;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_redacted(&mut stream, &event_id, message_position).await;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_remote_added(&mut stream, user_id, &event_id, &reaction, message_position).await;

        // Redact, add, redact, add, redact
        join_all((0..5).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;
        assert_redacted(&mut stream, &event_id, message_position).await;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_redacted(&mut stream, &event_id, message_position).await;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_redacted(&mut stream, &event_id, message_position).await;

        // Add, redact, add, redact
        join_all((0..4).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_redacted(&mut stream, &event_id, message_position).await;
        assert_local_added(&mut stream, user_id, &event_id, &reaction, message_position).await;
        assert_redacted(&mut stream, &event_id, message_position).await;
    }

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

async fn sync_room(client: &Client, room_id: &RoomId) -> Result<()> {
    client
        .sync_with_callback(SyncSettings::default(), |response| async move {
            if response.rooms.join.iter().any(|(id, _)| id == room_id) {
                LoopCtrl::Break
            } else {
                LoopCtrl::Continue
            }
        })
        .await?;
    Ok(())
}

async fn assert_event_is_updated(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    event_id: &EventId,
    index: usize,
) -> EventTimelineItem {
    let (i, event) = assert_matches!(
        stream.next().await,
        Some(VectorDiff::Set { index, value }) => (index, value)
    );
    assert_eq!(i, index);
    let event = event.as_event().unwrap();
    assert_eq!(event.event_id().unwrap(), event_id);
    event.to_owned()
}
