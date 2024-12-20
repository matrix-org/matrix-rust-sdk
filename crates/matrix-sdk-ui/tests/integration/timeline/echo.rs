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
use matrix_sdk::{
    assert_next_matches_with_timeout, config::SyncSettings, executor::spawn,
    ruma::MilliSecondsSinceUnixEpoch, test_utils::logged_in_client_with_server,
};
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, mocks::mock_encryption_state, JoinedRoomBuilder,
    SyncResponseBuilder,
};
use matrix_sdk_ui::timeline::{EventSendState, RoomExt, TimelineItemContent};
use ruma::{
    event_id,
    events::room::message::{MessageType, RoomMessageEventContent},
    room_id, uint, user_id,
};
use serde_json::json;
use stream_assert::assert_next_matches;
use tokio::task::yield_now;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::mock_sync;

#[async_test]
async fn test_echo() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(
        room.timeline_builder()
            .with_internal_id_prefix("le_prefix".to_owned())
            .build()
            .await
            .unwrap(),
    );
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$ev");

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "event_id": event_id })))
        .mount(&server)
        .await;

    // Don't move the original timeline, it must live until the end of the test
    let timeline = timeline.clone();
    #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
    let send_hdl = spawn(async move {
        timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await
    });

    assert_let!(Some(VectorDiff::PushBack { value: local_echo }) = timeline_stream.next().await);
    let item = local_echo.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));
    assert_let!(TimelineItemContent::Message(msg) = item.content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "Hello, World!");
    assert!(item.event_id().is_none());
    let txn_id = item.transaction_id().unwrap();

    assert_let!(Some(VectorDiff::PushFront { value: date_divider }) = timeline_stream.next().await);
    assert!(date_divider.is_date_divider());

    // Wait for the sending to finish and assert everything was successful
    send_hdl.await.unwrap().unwrap();

    assert_let!(
        Some(VectorDiff::Set { index: 1, value: sent_confirmation }) = timeline_stream.next().await
    );
    let item = sent_confirmation.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::Sent { .. }));
    assert_eq!(item.event_id(), Some(event_id));

    let f = EventFactory::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(
            f.text_msg("Hello, World!")
                .sender(user_id!("@example:localhost"))
                .event_id(event_id)
                .server_ts(152038280)
                .unsigned_transaction_id(txn_id),
        ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Local echo is replaced with the remote echo.
    assert_next_matches!(timeline_stream, VectorDiff::Remove { index: 1 });
    let remote_echo =
        assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => value);
    let item = remote_echo.as_event().unwrap();
    assert!(item.is_own());
    assert_eq!(item.timestamp(), MilliSecondsSinceUnixEpoch(uint!(152038280)));

    // The date divider is also replaced.
    let date_divider =
        assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());
    assert_next_matches!(timeline_stream, VectorDiff::Remove { index: 2 });
}

#[async_test]
async fn test_retry_failed() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    client.send_queue().set_enabled(true).await;
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

    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await.unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    // First, local echo is added.
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.send_state(), Some(EventSendState::NotSentYet));
    });

    // Sending fails, because the error is a transient one that's recoverable,
    // indicating something's wrong on the client side.
    assert_let!(Some(VectorDiff::Set { index: 0, value: item }) = timeline_stream.next().await);
    assert_matches!(
        item.send_state(),
        Some(EventSendState::SendingFailed { is_recoverable: true, .. })
    );

    // This doesn't disable the send queue at the global level…
    assert!(client.send_queue().is_enabled());
    // …but does so at the local level.
    assert!(!room.send_queue().is_enabled());

    // Have the endpoint return a success result, and re-enable the queue.
    drop(scoped_faulty_send);
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "event_id": "$wWgymRfo7ri1uQx0NXO40vLJ" })),
        )
        .mount(&server)
        .await;

    room.send_queue().set_enabled(true);

    // Let the send queue handle the event.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // After mocking the endpoint and retrying, it succeeds.
    assert_let!(Some(VectorDiff::Set { index: 0, value }) = timeline_stream.next().await);
    assert_matches!(value.send_state(), Some(EventSendState::Sent { .. }));
}

#[async_test]
async fn test_dedup_by_event_id_late() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$wWgymRfo7ri1uQx0NXO40vLJ");

    mock_encryption_state(&server, false).await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "event_id": event_id }))
                // Not great to use a timer for this, but it's what wiremock gives us right now.
                // Ideally we'd wait on a channel to produce a value or sth. like that, but
                // wiremock doesn't allow to handle multiple queries at the same time.
                .set_delay(Duration::from_millis(500)),
        )
        .mount(&server)
        .await;

    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await.unwrap();

    // Timeline: [local echo]
    let local_echo =
        assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => value);
    let item = local_echo.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

    // Timeline: [date-divider, local echo]
    let date_divider = assert_next_matches_with_timeout!( timeline_stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    let f = EventFactory::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(
            // Note: no transaction id.
            f.text_msg("Hello, World!")
                .sender(user_id!("@example:localhost"))
                .event_id(event_id)
                .server_ts(123456),
        ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    // Timeline: [remote-echo, date-divider, local echo]
    let remote_echo =
        assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => value);
    let item = remote_echo.as_event().unwrap();
    assert_eq!(item.event_id(), Some(event_id));

    // Timeline: [date-divider, remote-echo, date-divider, local echo]
    let date_divider = assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    // Local echo and its date divider are removed.
    // Timeline: [date-divider, remote-echo, date-divider]
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 3 }));
    // Timeline: [date-divider, remote-echo]
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 2 }));
}

#[async_test]
async fn test_cancel_failed() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let handle =
        timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await.unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    // Local echo is added (immediately)
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.send_state(), Some(EventSendState::NotSentYet));
    });

    // Sending fails, the mock server has no matching route
    assert_let!(Some(VectorDiff::Set { index: 0, value }) = timeline_stream.next().await);
    assert_matches!(value.send_state(), Some(EventSendState::SendingFailed { .. }));

    // Discard, assert the local echo is found
    assert!(handle.abort().await.unwrap());

    // Observable local echo being removed
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 0 }));
}
