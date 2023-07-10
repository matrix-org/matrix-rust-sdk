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
use matrix_sdk::{config::SyncSettings, executor::spawn, ruma::MilliSecondsSinceUnixEpoch};
use matrix_sdk_test::{async_test, EventBuilder, JoinedRoomBuilder, TimelineTestEvent};
use matrix_sdk_ui::timeline::{
    EventSendState, RoomExt, TimelineItemContent, TimelineItemKind, VirtualTimelineItem,
};
use ruma::{
    event_id,
    events::room::message::{MessageType, RoomMessageEventContent},
    room_id, uint, TransactionId,
};
use serde_json::json;
use stream_assert::assert_next_matches;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_encryption_state, mock_sync};

#[async_test]
async fn echo() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$wWgymRfo7ri1uQx0NXO40vLJ");
    let txn_id: &TransactionId = "my-txn-id".into();

    mock_encryption_state(&server, false).await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&json!({ "event_id": event_id })))
        .mount(&server)
        .await;

    // Don't move the original timeline, it must live until the end of the test
    let timeline = timeline.clone();
    #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
    let send_hdl = spawn(async move {
        timeline
            .send(RoomMessageEventContent::text_plain("Hello, World!").into(), Some(txn_id))
            .await
    });

    let _day_divider = assert_matches!(timeline_stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let local_echo = assert_matches!(timeline_stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = local_echo.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

    let msg = assert_matches!(item.content(), TimelineItemContent::Message(msg) => msg);
    let text = assert_matches!(msg.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "Hello, World!");

    // Wait for the sending to finish and assert everything was successful
    send_hdl.await.unwrap();

    let sent_confirmation = assert_matches!(
        timeline_stream.next().await,
        Some(VectorDiff::Set { index: 1, value }) => value
    );
    let item = sent_confirmation.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::Sent { .. }));

    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "body": "Hello, World!",
                "msgtype": "m.text",
            },
            "event_id": "$7at8sd:localhost",
            "origin_server_ts": 152038280,
            "sender": "@example:localhost",
            "type": "m.room.message",
            "unsigned": { "transaction_id": txn_id, },
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Local echo is removed
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 1 }));
    // Local echo day divider is removed
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 0 }));

    // New day divider is added
    let new_item = assert_matches!(
        timeline_stream.next().await,
        Some(VectorDiff::PushBack { value }) => value
    );
    assert_matches!(**new_item, TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_)));

    // Remote echo is added
    let remote_echo = assert_matches!(
        timeline_stream.next().await,
        Some(VectorDiff::PushBack { value }) => value
    );
    let item = remote_echo.as_event().unwrap();
    assert!(item.is_own());
    assert_eq!(item.timestamp(), MilliSecondsSinceUnixEpoch(uint!(152038280)));
}

#[async_test]
async fn retry_failed() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let event_id = event_id!("$wWgymRfo7ri1uQx0NXO40vLJ");
    let txn_id: &TransactionId = "my-txn-id".into();

    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into(), Some(txn_id)).await;

    // First, local echo is added
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.send_state(), Some(EventSendState::NotSentYet));
    });

    // Sending fails, the mock server has no matching route yet
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 0, value }) => {
        assert_matches!(value.send_state(), Some(EventSendState::SendingFailed { .. }));
    });

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&json!({ "event_id": event_id })))
        .mount(&server)
        .await;

    timeline.retry_send(txn_id).await.unwrap();

    // After mocking the endpoint and retrying, it first transitions back out of
    // the error state
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => {
        assert_matches!(value.send_state(), Some(EventSendState::NotSentYet));
    });

    // … before succeeding.
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 0, value }) => {
        assert_matches!(value.send_state(), Some(EventSendState::Sent { .. }));
    });
}

#[async_test]
async fn dedup_by_event_id_late() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$wWgymRfo7ri1uQx0NXO40vLJ");
    let txn_id: &TransactionId = "my-txn-id".into();

    mock_encryption_state(&server, false).await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&json!({ "event_id": event_id }))
                // Not great to use a timer for this, but it's what wiremock gives us right now.
                // Ideally we'd wait on a channel to produce a value or sth. like that.
                .set_delay(Duration::from_millis(100)),
        )
        .mount(&server)
        .await;

    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into(), Some(txn_id)).await;

    assert_matches!(timeline_stream.next().await, Some(VectorDiff::PushBack { .. })); // day divider
    let local_echo = assert_matches!(timeline_stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = local_echo.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "body": "Hello, World!",
                "msgtype": "m.text",
            },
            "event_id": event_id,
            "origin_server_ts": 123456,
            "sender": "@example:localhost",
            "type": "m.room.message",
            // no transaction ID
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    assert_next_matches!(timeline_stream, VectorDiff::Insert { index: 0, .. }); // day divider
    let remote_echo =
        assert_next_matches!(timeline_stream, VectorDiff::Insert { index: 1, value } => value);
    let item = remote_echo.as_event().unwrap();
    assert_eq!(item.event_id(), Some(event_id));

    // Local echo and its day divider are removed.
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 3 }));
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 2 }));
}

#[async_test]
async fn cancel_failed() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let txn_id: &TransactionId = "my-txn-id".into();

    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into(), Some(txn_id)).await;

    // Local echo is added (immediately)
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.send_state(), Some(EventSendState::NotSentYet));
    });

    // Sending fails, the mock server has no matching route
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 0, value }) => {
        assert_matches!(value.send_state(), Some(EventSendState::SendingFailed { .. }));
    });

    // Discard, assert the local echo is found
    assert!(timeline.cancel_send(txn_id).await);

    // Observable local echo being removed
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 0 }));
}
