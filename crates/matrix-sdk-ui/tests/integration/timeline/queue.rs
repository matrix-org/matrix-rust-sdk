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

use std::{
    future::pending,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
    time::Duration,
};

use assert_matches::assert_matches;
use axum::{http::StatusCode, response::IntoResponse, routing::put, Json};
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{async_test, JoinedRoomBuilder, SyncResponseBuilder, TimelineTestEvent};
use matrix_sdk_ui::timeline::{EventItemOrigin, EventSendState, RoomExt};
use ruma::{events::room::message::RoomMessageEventContent, room_id};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use tokio::{
    task::yield_now,
    time::{sleep, timeout},
};

use crate::axum::{logged_in_client, RouterExt};

#[async_test]
async fn message_order() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let sync_builder = SyncResponseBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let (fst_tx, fst_rx) = async_channel::bounded(1);
    let (snd_tx, snd_rx) = async_channel::bounded(1);
    let client = logged_in_client(
        axum::Router::new()
            .route(
                "/_matrix/client/v3/rooms/:room_id/send/*rest",
                put(move |body: String| async move {
                    if body.contains("First!") {
                        fst_rx.recv().await.unwrap();
                        Json(json!({ "event_id": "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP" }))
                    } else if body.contains("Second.") {
                        snd_rx.recv().await.unwrap();
                        Json(json!({ "event_id": "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ" }))
                    } else {
                        unreachable!()
                    }
                }),
            )
            .mock_encryption_state(false)
            .mock_sync_responses(sync_builder.clone()),
    )
    .await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    client.sync_once(sync_settings.clone()).await.unwrap();

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    timeline.send(RoomMessageEventContent::text_plain("First!").into(), None).await;
    timeline.send(RoomMessageEventContent::text_plain("Second.").into(), None).await;

    // Local echoes are available as soon as `timeline.send` returns
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
    });

    // Make both requests succeed at this point, but in reverse order.
    snd_tx.send(()).await.unwrap();
    yield_now().await;
    fst_tx.send(()).await.unwrap();

    let retries = async move {
        // The first item should be updated first
        assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 0, value }) => {
            assert_eq!(value.content().as_message().unwrap().body(), "First!");
            assert_eq!(value.event_id().unwrap(), "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP");
        });
        // Then the second one
        assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 1, value }) => {
            assert_eq!(value.content().as_message().unwrap().body(), "Second.");
            assert_eq!(value.event_id().unwrap(), "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ");
        });
        assert_pending!(timeline_stream);
    };

    timeout(Duration::from_millis(500), retries).await.unwrap();
}

#[async_test]
async fn retry_order() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let sync_builder = SyncResponseBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let num_requests = Arc::new(AtomicU8::new(0));
    let client = logged_in_client(
        axum::Router::new()
            .route(
                "/_matrix/client/v3/rooms/:room_id/send/*rest",
                put(move |body: String| async move {
                    // The first request fails
                    if num_requests.fetch_add(1, Ordering::AcqRel) < 1 {
                        // Make sure not to return immediately, so both requests have made it to
                        // the queue before the first one fails.
                        sleep(Duration::from_millis(50)).await;
                        return StatusCode::BAD_REQUEST.into_response();
                    }

                    // Following requests succeed
                    if body.contains("First!") {
                        // Response for first message retry takes 50ms to respond
                        sleep(Duration::from_millis(50)).await;
                        Json(json!({ "event_id": "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP" }))
                            .into_response()
                    } else if body.contains("Second.") {
                        // Response for second message retry takes 100ms to respond,
                        // so should come back after first if we don't serialize retries
                        sleep(Duration::from_millis(100)).await;
                        Json(json!({ "event_id": "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ" }))
                            .into_response()
                    } else {
                        unreachable!()
                    }
                }),
            )
            .mock_encryption_state(false)
            .mock_sync_responses(sync_builder.clone()),
    )
    .await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    client.sync_once(sync_settings.clone()).await.unwrap();

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    // Try sending two messages
    timeline.send(RoomMessageEventContent::text_plain("First!").into(), Some("1".into())).await;
    timeline.send(RoomMessageEventContent::text_plain("Second.").into(), Some("2".into())).await;

    // Local echoes are available as soon as `timeline.send` returns
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
    });

    // Local echoes are updated with the failed send state as soon as
    // the error response is received
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 0, value }) => {
        assert_matches!(value.send_state().unwrap(), EventSendState::SendingFailed { .. });
    });
    // The second one is cancelled without an extra delay
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert_matches!(value.send_state().unwrap(), EventSendState::Cancelled);
    });

    // Retry both messages, but the second one first
    timeline.retry_send("2".into()).await.unwrap();
    timeline.retry_send("1".into()).await.unwrap();

    // Both items are immediately updated and moved to the bottom in the order
    // of the function calls to indicate they are being sent
    assert_next_matches!(timeline_stream, VectorDiff::Remove { index: 1 });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.send_state().unwrap(), EventSendState::NotSentYet);
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
    });
    assert_next_matches!(timeline_stream, VectorDiff::Remove { index: 0 });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.send_state().unwrap(), EventSendState::NotSentYet);
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
    });

    let retries = async move {
        // The second item (now at index 0) should be updated first, since it was
        // retried first
        assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 0, value }) => {
            assert_eq!(value.content().as_message().unwrap().body(), "Second.");
            assert_matches!(value.send_state().unwrap(), EventSendState::Sent { .. });
            assert_eq!(value.event_id().unwrap(), "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ");
        });
        // Then the first one
        assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 1, value }) => {
            assert_eq!(value.content().as_message().unwrap().body(), "First!");
            assert_matches!(value.send_state().unwrap(), EventSendState::Sent { .. });
            assert_eq!(value.event_id().unwrap(), "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP");
        });
    };

    timeout(Duration::from_millis(500), retries).await.unwrap();
}

#[async_test]
async fn clear_with_echoes() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let sync_builder = SyncResponseBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let num_requests = Arc::new(AtomicU8::new(0));
    let client = logged_in_client(
        axum::Router::new()
            .route(
                "/_matrix/client/v3/rooms/:room_id/send/*rest",
                put(|| async move {
                    if num_requests.fetch_add(1, Ordering::AcqRel) < 1 {
                        // First request fails
                        StatusCode::BAD_REQUEST
                    } else {
                        // Second request stalls forever
                        pending().await
                    }
                }),
            )
            .mock_encryption_state(false)
            .mock_sync_responses(sync_builder.clone()),
    )
    .await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    client.sync_once(sync_settings.clone()).await.unwrap();

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;

    {
        let (_, mut timeline_stream) = timeline.subscribe().await;

        // Send the first message (will fail)
        timeline.send(RoomMessageEventContent::text_plain("Send failure").into(), None).await;

        // Wait for the first message to fail. Don't use time, but listen for the first
        // timeline item diff to get back signalling the error.
        let _day_divider = timeline_stream.next().await;
        let _local_echo = timeline_stream.next().await;
        let _local_echo_replaced_with_failure = timeline_stream.next().await;
    }

    // Send the second message (will stall forever in the server)
    timeline.send(RoomMessageEventContent::text_plain("Pending").into(), None).await;

    // Another message comes in.
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(TimelineTestEvent::MessageText),
    );
    client.sync_once(sync_settings.clone()).await.unwrap();

    // At this point, there should be three timeline items:
    let timeline_items = timeline.items().await;
    let event_items: Vec<_> = timeline_items.iter().filter_map(|item| item.as_event()).collect();

    assert_eq!(event_items.len(), 3);
    // The message that failed to send.
    assert_matches!(event_items[0].send_state(), Some(EventSendState::SendingFailed { .. }));
    // The message that came in from sync.
    assert_matches!(event_items[1].origin(), Some(EventItemOrigin::Sync));
    // The message that is still pending.
    assert_matches!(event_items[2].send_state(), Some(EventSendState::NotSentYet));

    // When we clear the timeline now,
    timeline.clear().await;

    // … the two local messages should remain.
    let timeline_items = timeline.items().await;
    let event_items: Vec<_> = timeline_items.iter().filter_map(|item| item.as_event()).collect();

    assert_eq!(event_items.len(), 2);
    assert_matches!(event_items[0].send_state(), Some(EventSendState::SendingFailed { .. }));
    assert_matches!(event_items[1].send_state(), Some(EventSendState::NotSentYet));
}
