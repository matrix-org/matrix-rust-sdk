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

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{async_test, EventBuilder, JoinedRoomBuilder};
use matrix_sdk_ui::timeline::{EventSendState, RoomExt};
use ruma::{events::room::message::RoomMessageEventContent, room_id};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use tokio::time::sleep;
use wiremock::{
    matchers::{body_string_contains, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_encryption_state, mock_sync};

#[async_test]
async fn message_order() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    // Response for first message takes 200ms to respond
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(body_string_contains("First!"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&json!({ "event_id": "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP" }))
                .set_delay(Duration::from_millis(200)),
        )
        .mount(&server)
        .await;

    // Response for second message only takes 100ms to respond, so should come
    // back first if we don't serialize requests
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(body_string_contains("Second."))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&json!({ "event_id": "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ" }))
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;

    timeline.send(RoomMessageEventContent::text_plain("First!").into(), None).await;
    timeline.send(RoomMessageEventContent::text_plain("Second.").into(), None).await;

    // Local echoes are available as soon as `timeline.send` returns
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
    });

    // Wait 200ms for the first msg, 100ms for the second, 100ms for overhead
    sleep(Duration::from_millis(400)).await;

    // The first item should be updated first
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
        assert_eq!(value.event_id().unwrap(), "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP");
    });
    // Then the second one
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
        assert_eq!(value.event_id().unwrap(), "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ");
    });
    assert_pending!(timeline_stream);
}

#[async_test]
async fn retry_order() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    // Send two messages without mocking the server response.
    // It will respond with a 404, resulting in a failed-to-send state.
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
    // the 404 response is received
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 0, value }) => {
        assert_matches!(value.send_state().unwrap(), EventSendState::SendingFailed { .. });
    });
    // The second one is cancelled without an extra delay
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert_matches!(value.send_state().unwrap(), EventSendState::Cancelled);
    });

    // Response for first message takes 100ms to respond
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(body_string_contains("First!"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&json!({ "event_id": "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP" }))
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;

    // Response for second message takes 200ms to respond, so should come back
    // after first if we don't serialize retries
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(body_string_contains("Second."))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&json!({ "event_id": "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ" }))
                .set_delay(Duration::from_millis(200)),
        )
        .mount(&server)
        .await;

    // Retry the second message first
    timeline.retry_send("2".into()).await.unwrap();
    timeline.retry_send("1".into()).await.unwrap();

    // Both items are immediately updated to indicate they are being sent
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert_matches!(value.send_state().unwrap(), EventSendState::NotSentYet);
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
    });
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => {
        assert_matches!(value.send_state().unwrap(), EventSendState::NotSentYet);
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
    });

    // Wait 200ms for the first msg, 100ms for the second, 200ms for overhead
    sleep(Duration::from_millis(500)).await;

    // The second item should be updated first, since it was retried first
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
        assert_matches!(value.send_state().unwrap(), EventSendState::Sent { .. });
        assert_eq!(value.event_id().unwrap(), "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ");
    });
    // Then the first one
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
        assert_matches!(value.send_state().unwrap(), EventSendState::Sent { .. });
        assert_eq!(value.event_id().unwrap(), "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP");
    });
    assert_pending!(timeline_stream);
}
