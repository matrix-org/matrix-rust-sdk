// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Tests specific to a timeline focused on an event.

use std::time::Duration;

use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{config::SyncSettings, test_utils::logged_in_client_with_server};
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, mocks::mock_encryption_state, JoinedRoomBuilder,
    SyncResponseBuilder, ALICE, BOB,
};
use matrix_sdk_ui::timeline::{TimelineBuilder, TimelineFocus};
use ruma::{event_id, events::room::message::RoomMessageEventContent, room_id};
use stream_assert::assert_pending;
use tokio::time::sleep;

use crate::{mock_context, mock_messages, mock_sync};

#[async_test]
async fn test_new_focused() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_response_builder = SyncResponseBuilder::new();
    sync_response_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    // Mark the room as joined.
    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let f = EventFactory::new().room(room_id);
    let target_event = event_id!("$1");

    mock_context(
        &server,
        room_id,
        target_event,
        Some("prev1".to_owned()),
        vec![
            f.text_msg("i tried so hard").sender(*ALICE).into_event(),
            f.text_msg("and got so far").sender(*ALICE).into_event(),
        ],
        f.text_msg("in the end").event_id(target_event).sender(*BOB).into_event(),
        vec![
            f.text_msg("it doesn't even").sender(*ALICE).into_event(),
            f.text_msg("matter").sender(*ALICE).into_event(),
        ],
        Some("next1".to_owned()),
        vec![],
    )
    .await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = TimelineBuilder::new(&room)
        .with_focus(TimelineFocus::Event {
            target: target_event.to_owned(),
            num_context_events: 20,
            hide_threaded_events: false,
        })
        .build()
        .await
        .unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a focused timeline"
    );

    server.reset().await;

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 5 + 1); // event items + a date divider
    assert!(items[0].is_date_divider());
    assert_eq!(
        items[1].as_event().unwrap().content().as_message().unwrap().body(),
        "i tried so hard"
    );
    assert_eq!(
        items[2].as_event().unwrap().content().as_message().unwrap().body(),
        "and got so far"
    );
    assert_eq!(items[3].as_event().unwrap().content().as_message().unwrap().body(), "in the end");
    assert_eq!(
        items[4].as_event().unwrap().content().as_message().unwrap().body(),
        "it doesn't even"
    );
    assert_eq!(items[5].as_event().unwrap().content().as_message().unwrap().body(), "matter");

    assert_pending!(timeline_stream);

    // Now trigger a backward pagination.
    mock_messages(
        &server,
        "prev1".to_owned(),
        None,
        vec![
            // reversed manually here
            f.text_msg("And even though I tried, it all fell apart").sender(*BOB).into_event(),
            f.text_msg("I kept everything inside").sender(*BOB).into_event(),
        ],
        vec![],
    )
    .await;

    let hit_start = timeline.paginate_backwards(20).await.unwrap();
    assert!(hit_start);

    server.reset().await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 4);

    assert_let!(VectorDiff::PushFront { value: message } = &timeline_updates[0]);
    assert_eq!(
        message.as_event().unwrap().content().as_message().unwrap().body(),
        "And even though I tried, it all fell apart"
    );

    assert_let!(VectorDiff::PushFront { value: message } = &timeline_updates[1]);
    assert_eq!(
        message.as_event().unwrap().content().as_message().unwrap().body(),
        "I kept everything inside"
    );

    // Date divider post processing.
    assert_let!(VectorDiff::PushFront { value: item } = &timeline_updates[2]);
    assert!(item.is_date_divider());

    assert_let!(VectorDiff::Remove { index } = &timeline_updates[3]);
    assert_eq!(*index, 3);

    // Now trigger a forward pagination.
    mock_messages(
        &server,
        "next1".to_owned(),
        Some("next2".to_owned()),
        vec![
            f.text_msg("I had to fall, to lose it all").sender(*BOB).into_event(),
            f.text_msg("But in the end, it doesn't event matter").sender(*BOB).into_event(),
        ],
        vec![],
    )
    .await;

    let hit_start = timeline.paginate_forwards(20).await.unwrap();
    assert!(!hit_start); // because we gave it another next2 token.

    server.reset().await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
    assert_eq!(
        message.as_event().unwrap().content().as_message().unwrap().body(),
        "I had to fall, to lose it all"
    );

    assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[1]);
    assert_eq!(
        message.as_event().unwrap().content().as_message().unwrap().body(),
        "But in the end, it doesn't event matter"
    );

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_live_aggregations_are_reflected_on_focused_timelines() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_response_builder = SyncResponseBuilder::new();
    sync_response_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    // Mark the room as joined.
    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Start a focused timeline.
    let f = EventFactory::new().room(room_id);
    let target_event = event_id!("$1");

    mock_context(
        &server,
        room_id,
        target_event,
        None,
        vec![],
        f.text_msg("yolo").event_id(target_event).sender(*BOB).into_event(),
        vec![],
        None,
        vec![],
    )
    .await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = TimelineBuilder::new(&room)
        .with_focus(TimelineFocus::Event {
            target: target_event.to_owned(),
            num_context_events: 20,
            hide_threaded_events: false,
        })
        .build()
        .await
        .unwrap();

    server.reset().await;

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // event items + a date divider
    assert!(items[0].is_date_divider());

    let event_item = items[1].as_event().unwrap();
    assert_eq!(event_item.content().as_message().unwrap().body(), "yolo");
    assert_eq!(event_item.content().reactions().cloned().unwrap_or_default().len(), 0);

    assert_pending!(timeline_stream);

    // Now simulate a sync that returns a new message-like event, and a reaction
    // to the $1 event.
    sync_response_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_bulk([
        // This event must be ignored.
        f.text_msg("this is a sync event").sender(*ALICE).into(),
        // This event must not be ignored.
        f.reaction(target_event, "👍").sender(*BOB).into(),
    ]));

    // Sync the room.
    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // We only receive one updated for the reaction, from the timeline stream.
    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

    let event_item = item.as_event().unwrap();
    assert_eq!(event_item.content().as_message().unwrap().body(), "yolo");
    let reactions = event_item.content().reactions().cloned().unwrap_or_default();
    assert_eq!(reactions.len(), 1);
    let _ = reactions["👍"][*BOB];
}

#[async_test]
async fn test_focused_timeline_local_echoes() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_response_builder = SyncResponseBuilder::new();
    sync_response_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    // Mark the room as joined.
    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Start a focused timeline.
    let f = EventFactory::new().room(room_id);
    let target_event = event_id!("$1");

    mock_context(
        &server,
        room_id,
        target_event,
        None,
        vec![],
        f.text_msg("yolo").event_id(target_event).sender(*BOB).into_event(),
        vec![],
        None,
        vec![],
    )
    .await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = TimelineBuilder::new(&room)
        .with_focus(TimelineFocus::Event {
            target: target_event.to_owned(),
            num_context_events: 20,
            hide_threaded_events: false,
        })
        .build()
        .await
        .unwrap();

    server.reset().await;

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // event items + a date divider
    assert!(items[0].is_date_divider());

    let event_item = items[1].as_event().unwrap();
    assert_eq!(event_item.content().as_message().unwrap().body(), "yolo");
    assert_eq!(event_item.content().reactions().cloned().unwrap_or_default().len(), 0);

    sleep(Duration::from_millis(100)).await;
    assert_pending!(timeline_stream);

    // Add a reaction to the focused event, which will cause a local echo to happen.
    timeline.toggle_reaction(&event_item.identifier(), "✨").await.unwrap();

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // We immediately get the local echo for the reaction.
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);

    let event_item = item.as_event().unwrap();
    // Text hasn't changed.
    assert_eq!(event_item.content().as_message().unwrap().body(), "yolo");
    // But now there's one reaction to the event.
    let reactions = event_item.content().reactions().cloned().unwrap_or_default();
    assert_eq!(reactions.len(), 1);
    assert!(reactions.get("✨").unwrap().get(client.user_id().unwrap()).is_some());

    // And nothing more.
    sleep(Duration::from_millis(100)).await;
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_focused_timeline_doesnt_show_local_echoes() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_response_builder = SyncResponseBuilder::new();
    sync_response_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    // Mark the room as joined.
    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Start a focused timeline.
    let f = EventFactory::new().room(room_id);
    let target_event = event_id!("$1");

    mock_context(
        &server,
        room_id,
        target_event,
        None,
        vec![],
        f.text_msg("yolo").event_id(target_event).sender(*BOB).into_event(),
        vec![],
        None,
        vec![],
    )
    .await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = TimelineBuilder::new(&room)
        .with_focus(TimelineFocus::Event {
            target: target_event.to_owned(),
            num_context_events: 20,
            hide_threaded_events: false,
        })
        .build()
        .await
        .unwrap();

    server.reset().await;

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // event items + a date divider
    assert!(items[0].is_date_divider());

    let event_item = items[1].as_event().unwrap();
    assert_eq!(event_item.content().as_message().unwrap().body(), "yolo");
    assert_eq!(event_item.content().reactions().cloned().unwrap_or_default().len(), 0);

    assert_pending!(timeline_stream);

    // Send a message in the room, expect no local echo.
    timeline.send(RoomMessageEventContent::text_plain("h4xx0r").into()).await.unwrap();

    // Let a bit of time for the send queue to process the event.
    sleep(Duration::from_millis(300)).await;

    // And nothing more.
    assert_pending!(timeline_stream);
}
