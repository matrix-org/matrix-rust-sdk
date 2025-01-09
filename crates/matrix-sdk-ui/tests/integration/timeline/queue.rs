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
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{config::SyncSettings, test_utils::logged_in_client_with_server, Error};
use matrix_sdk_base::store::QueueWedgeError;
use matrix_sdk_test::{
    async_test, mocks::mock_encryption_state, EventBuilder, JoinedRoomBuilder, SyncResponseBuilder,
    ALICE,
};
use matrix_sdk_ui::timeline::{EventItemOrigin, EventSendState, RoomExt};
use ruma::{
    event_id, events::room::message::RoomMessageEventContent, room_id, MilliSecondsSinceUnixEpoch,
};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use tokio::{task::yield_now, time::sleep};
use wiremock::{
    matchers::{body_string_contains, header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::mock_sync;

#[async_test]
async fn test_message_order() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_response_builder = SyncResponseBuilder::new();
    sync_response_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    // Response for first message takes 200ms to respond
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(body_string_contains("First!"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "event_id": "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP" }))
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
                .set_body_json(json!({ "event_id": "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ" }))
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;

    timeline.send(RoomMessageEventContent::text_plain("First!").into()).await.unwrap();
    timeline.send(RoomMessageEventContent::text_plain("Second.").into()).await.unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    // Local echoes are available after the send queue has processed these.
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert!(value.is_editable(), "local echo of first can be edited");
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert!(value.is_editable(), "local echo of second can be edited");
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
    });

    // Wait 200ms for the first msg, 100ms for the second, 200ms for overhead.
    sleep(Duration::from_millis(500)).await;

    // The first item should be updated first.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => {
        assert!(value.is_editable(), "remote echo of first can be edited");
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
        assert_eq!(value.event_id().unwrap(), "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP");
    });

    // Then the second one.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert!(value.is_editable(), "remote echo of second can be edited");
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
        assert_eq!(value.event_id().unwrap(), "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ");
    });

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_retry_order() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_response_builder = SyncResponseBuilder::new();
    sync_response_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    // When trying to send an event, return with a 500 error, which is interpreted
    // as a transient error.
    server.reset().await;
    mock_encryption_state(&server, false).await;
    let scoped_faulty_send = Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(500))
        .expect(3)
        .mount_as_scoped(&server)
        .await;

    // Send two messages without mocking the server response.
    // It will respond with a 500, resulting in a failed-to-send state.
    timeline.send(RoomMessageEventContent::text_plain("First!").into()).await.unwrap();
    timeline.send(RoomMessageEventContent::text_plain("Second.").into()).await.unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    // Local echoes are available after the send queue has processed these.
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
    });

    // Local echoes are updated with the failed send state as soon as
    // the 404 response is received.
    assert_let!(Some(VectorDiff::Set { index: 0, value: first }) = timeline_stream.next().await);
    assert_matches!(first.send_state().unwrap(), EventSendState::SendingFailed { .. });

    // Response for first message takes 100ms to respond.
    drop(scoped_faulty_send);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(body_string_contains("First!"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "event_id": "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP" }))
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;

    // Response for second message takes 200ms to respond, so should come back
    // after first if we don't serialize retries.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(body_string_contains("Second."))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "event_id": "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ" }))
                .set_delay(Duration::from_millis(200)),
        )
        .mount(&server)
        .await;

    // Retry the second message first.
    client.send_queue().set_enabled(true).await;

    // Wait 200ms for the first msg, 100ms for the second, 300ms for overhead.
    sleep(Duration::from_millis(600)).await;

    // With the send queue, sending is retried in the same order as the events
    // were sent. So we first see the first message.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
        assert_matches!(value.send_state().unwrap(), EventSendState::Sent { .. });
        assert_eq!(value.event_id().unwrap(), "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP");
    });

    // Then the second.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
        assert_matches!(value.send_state().unwrap(), EventSendState::Sent { .. });
        assert_eq!(value.event_id().unwrap(), "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ");
    });

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_reloaded_failed_local_echoes_are_marked_as_failed() {
    let room_id = room_id!("!a98sd12bjh:example.org");

    let (client, server) = logged_in_client_with_server().await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_response_builder = SyncResponseBuilder::new();
    sync_response_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    // When trying to send an event, return with a 500 error, which is interpreted
    // as a transient error.
    server.reset().await;
    mock_encryption_state(&server, false).await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(413).set_body_json(json!({
            // From https://spec.matrix.org/v1.10/client-server-api/#standard-error-response
            "errcode": "M_TOO_LARGE",
            "error": "Sounds like you have a lot to say!"
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Sending an event will respond with a 500, resulting in a failed-to-send
    // state.
    timeline.send(RoomMessageEventContent::text_plain("wall of text").into()).await.unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    // Local echoes are available after the send queue has processed these.
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.content().as_message().unwrap().body(), "wall of text");
    });

    // Local echoes are updated with the failed send state as soon as the error
    // response has been received.
    assert_let!(Some(VectorDiff::Set { index: 0, value: first }) = timeline_stream.next().await);
    let (error, is_recoverable) = assert_matches!(first.send_state().unwrap(), EventSendState::SendingFailed { error, is_recoverable } => (error, is_recoverable));

    // The error is not recoverable.
    assert!(!is_recoverable);
    // And it's properly pattern-matched as an HTTP error.
    assert_matches!(
        error.as_client_api_error().unwrap().error_kind(),
        Some(ruma::api::client::error::ErrorKind::TooLarge)
    );

    assert_pending!(timeline_stream);

    // Recreating a new timeline will show the wedged local echo.
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (initial, _) = timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].content().as_message().unwrap().body(), "wall of text");
    assert_let!(
        Some(EventSendState::SendingFailed { error, is_recoverable }) = initial[0].send_state()
    );

    // Same recoverable status as above.
    assert!(!is_recoverable);
    // It was persisted and it can be matched as a string now.
    let msg = assert_matches!(
        &**error,
        Error::SendQueueWedgeError(QueueWedgeError::GenericApiError { msg }) => msg
    );
    assert_eq!(
        msg,
        "the server returned an error: [413 / M_TOO_LARGE] Sounds like you have a lot to say!"
    );
}

#[async_test]
async fn test_clear_with_echoes() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();

    // Send a message without mocking the server response.
    {
        let (_, mut timeline_stream) = timeline.subscribe().await;

        timeline.send(RoomMessageEventContent::text_plain("Send failure").into()).await.unwrap();

        // Wait for the first message to fail. Don't use time, but listen for the first
        // timeline item diff to get back signalling the error.
        let _date_divider = timeline_stream.next().await;
        let _local_echo = timeline_stream.next().await;
        let _local_echo_replaced_with_failure = timeline_stream.next().await;
    }

    // Next message will take "forever" to send.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "event_id": "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP" }))
                .set_delay(Duration::from_secs(3600)),
        )
        .mount(&server)
        .await;

    // (this one)
    timeline.send(RoomMessageEventContent::text_plain("Pending").into()).await.unwrap();

    // Another message comes in.
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        event_builder.make_sync_message_event(
            &ALICE,
            RoomMessageEventContent::text_plain("another message"),
        ),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(sync_settings.clone()).await.unwrap();

    // At this point, there should be three timeline items:
    let timeline_items = timeline.items().await;
    let event_items: Vec<_> = timeline_items.iter().filter_map(|item| item.as_event()).collect();

    assert_eq!(event_items.len(), 3);
    // The message that came in from sync.
    assert_matches!(event_items[0].origin(), Some(EventItemOrigin::Sync));
    // The message that failed to send.
    assert_matches!(event_items[1].send_state(), Some(EventSendState::SendingFailed { .. }));
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

#[async_test]
async fn test_no_duplicate_date_divider() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_response_builder = SyncResponseBuilder::new();
    sync_response_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) = timeline.subscribe().await;

    // Response for first message takes 200ms to respond.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(body_string_contains("First!"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "event_id": "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP",
                }))
                .set_delay(Duration::from_millis(200)),
        )
        .mount(&server)
        .await;

    // Response for second message only takes 100ms to respond, so should come
    // back first if we don't serialize requests.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(body_string_contains("Second."))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "event_id": "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ",
                }))
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;

    timeline.send(RoomMessageEventContent::text_plain("First!").into()).await.unwrap();
    timeline.send(RoomMessageEventContent::text_plain("Second.").into()).await.unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    // Local echoes are available as soon as `timeline.send` returns.
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().content().as_message().unwrap().body(), "First!");
    });

    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });

    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().content().as_message().unwrap().body(), "Second.");
    });

    // Wait 200ms for the first msg, 100ms for the second, 200ms for overhead.
    sleep(Duration::from_millis(500)).await;

    // The first item should be updated first.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        let value = value.as_event().unwrap();
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
        assert_eq!(value.event_id().unwrap(), "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP");
    });

    // Then the second one.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 2, value } => {
        let value = value.as_event().unwrap();
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
        assert_eq!(value.event_id().unwrap(), "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ");
    });

    assert_pending!(timeline_stream);

    // Now have the sync return both events with more data.
    let ev_builder = EventBuilder::new();

    let now = MilliSecondsSinceUnixEpoch::now();
    ev_builder.set_next_ts(now.0.into());

    sync_response_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(ev_builder.make_sync_message_event_with_id(
                client.user_id().unwrap(),
                event_id!("$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP"),
                RoomMessageEventContent::text_plain("First!"),
            ))
            .add_timeline_event(ev_builder.make_sync_message_event_with_id(
                client.user_id().unwrap(),
                event_id!("$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ"),
                RoomMessageEventContent::text_plain("Second."),
            )),
    );

    mock_sync(&server, sync_response_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // The first message is removed -> [DD Second]
    assert_next_matches!(timeline_stream, VectorDiff::Remove { index: 1 });

    // The first message is reinserted -> [First DD Second]
    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        let value = value.as_event().unwrap();
        assert_eq!(value.content().as_message().unwrap().body(), "First!");
        assert_eq!(value.event_id().unwrap(), "$PyHxV5mYzjetBUT3qZq7V95GOzxb02EP");
    });

    // The second message is replaced -> [First Second DD]
    assert_next_matches!(timeline_stream, VectorDiff::Remove { index: 2 });
    assert_next_matches!(timeline_stream, VectorDiff::Insert { index: 1, value } => {
        let value = value.as_event().unwrap();
        assert_eq!(value.content().as_message().unwrap().body(), "Second.");
        assert_eq!(value.event_id().unwrap(), "$5E2kLK/Sg342bgBU9ceEIEPYpbFaqJpZ");
    });

    // A new date divider is inserted -> [DD First Second DD]
    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });

    // The useless date divider is removed. -> [DD First Second]
    assert_next_matches!(timeline_stream, VectorDiff::Remove { index: 3 });

    assert_pending!(timeline_stream);
}
