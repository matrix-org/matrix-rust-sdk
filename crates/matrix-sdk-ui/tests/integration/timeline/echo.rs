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
use matrix_sdk::{executor::spawn, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{EventSendState, RoomExt};
use ruma::{
    event_id,
    events::room::message::{MessageType, RoomMessageEventContent},
    room_id, user_id,
};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use tokio::task::yield_now;
use wiremock::ResponseTemplate;

#[async_test]
async fn test_echo() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = Arc::new(
        room.timeline_builder()
            .with_internal_id_prefix("le_prefix".to_owned())
            .build()
            .await
            .unwrap(),
    );
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$ev");

    server.mock_room_send().ok(event_id).mock_once().mount().await;

    // Don't move the original timeline, it must live until the end of the test
    let timeline = timeline.clone();
    #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
    let send_hdl = spawn(async move {
        timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await
    });

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value: local_echo } = &timeline_updates[0]);
    let item = local_echo.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));
    assert_let!(Some(msg) = item.content().as_message());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "Hello, World!");
    assert!(item.event_id().is_none());
    let txn_id = item.transaction_id().unwrap();

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    // Wait for the sending to finish and assert everything was successful
    send_hdl.await.unwrap().unwrap();

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 5);

    // The `EventSendState` has been updated.
    assert_let!(VectorDiff::Set { index: 1, value: sent_confirmation } = &timeline_updates[0]);
    let item = sent_confirmation.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::Sent { .. }));
    assert_eq!(item.event_id(), Some(event_id));

    // The local event is removed.
    assert_matches!(&timeline_updates[1], VectorDiff::Remove { index: 1 });

    // The new event is inserted in the Event Cache: it comes as a remote event.
    assert_let!(VectorDiff::PushFront { value: remote_event } = &timeline_updates[2]);
    let item = remote_event.as_event().unwrap();
    assert_let!(Some(msg) = item.content().as_message());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "Hello, World!");
    assert_eq!(item.event_id(), Some(event_id));

    // The date divider is adjusted.
    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[3]);
    assert!(date_divider.is_date_divider());
    assert_matches!(&timeline_updates[4], VectorDiff::Remove { index: 2 });

    assert_pending!(timeline_stream);

    let another_event_id = event_id!("$ev1");
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("Hello, World!")
                        .sender(user_id!("@example:localhost"))
                        .event_id(event_id)
                        .server_ts(152038280)
                        .unsigned_transaction_id(txn_id),
                )
                .add_timeline_event(
                    f.text_msg("Raclette")
                        .sender(user_id!("@example:localhost"))
                        .event_id(another_event_id)
                        .server_ts(152038281),
                ),
        )
        .await;

    // The Event Cache deduplicates the first event, but we receive a second one.
    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 5);

    assert_matches!(&timeline_updates[0], VectorDiff::Remove { index: 1 });

    assert_let!(VectorDiff::PushFront { value: first_event } = &timeline_updates[1]);
    assert_eq!(first_event.as_event().unwrap().event_id(), Some(event_id));

    assert_let!(VectorDiff::Insert { index: 1, value: second_event } = &timeline_updates[2]);
    assert_eq!(second_event.as_event().unwrap().event_id(), Some(another_event_id));

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[3]);
    assert!(date_divider.is_date_divider());

    assert_matches!(&timeline_updates[4], VectorDiff::Remove { index: 3 });

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_retry_failed() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    client.send_queue().set_enabled(true).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    // When trying to send an event, return with a 500 error, which is interpreted
    // as a transient error.
    let scoped_faulty_send = server.mock_room_send().error500().expect(3).mount_as_scoped().await;

    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await.unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    // First, local echo is added.
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.send_state(), Some(EventSendState::NotSentYet { progress: None }));
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
    server.mock_room_send().ok(event_id!("$wWgymRfo7ri1uQx0NXO40vLJ")).mount().await;

    room.send_queue().set_enabled(true);

    // Let the send queue handle the event.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // After mocking the endpoint and retrying, it succeeds.
    assert_let!(Some(VectorDiff::Set { index: 0, value }) = timeline_stream.next().await);
    assert_matches!(value.send_state(), Some(EventSendState::Sent { .. }));
}

#[async_test]
async fn test_dedup_by_event_id_late() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$wWgymRfo7ri1uQx0NXO40vLJ");

    server
        .mock_room_send()
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "event_id": event_id }))
                // Not great to use a timer for this, but it's what wiremock gives us right now.
                // Ideally we'd wait on a channel to produce a value or sth. like that, but
                // wiremock doesn't allow to handle multiple queries at the same time.
                .set_delay(Duration::from_millis(500)),
        )
        .mount()
        .await;

    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await.unwrap();

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    // Timeline: [local echo]
    assert_let!(VectorDiff::PushBack { value: local_echo } = &timeline_updates[0]);
    let item = local_echo.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));

    // Timeline: [date-divider, local echo]
    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                // Note: no transaction id.
                f.text_msg("Hello, World!")
                    .sender(user_id!("@example:localhost"))
                    .event_id(event_id)
                    .server_ts(123456),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    // Timeline: [remote-echo, date-divider, local echo]
    assert_let!(VectorDiff::PushFront { value: remote_echo } = &timeline_updates[0]);
    let item = remote_echo.as_event().unwrap();
    assert_eq!(item.event_id(), Some(event_id));

    // Timeline: [date-divider, remote-echo, date-divider, local echo]
    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    // Local echo and its date divider are removed.
    // Timeline: [date-divider, remote-echo, date-divider]
    assert_let!(VectorDiff::Remove { index: 3 } = &timeline_updates[0]);

    // Timeline: [date-divider, remote-echo]
    assert_let!(VectorDiff::Remove { index: 2 } = &timeline_updates[1]);

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_cancel_failed() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let handle =
        timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await.unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    // Local echo is added (immediately)
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.send_state(), Some(EventSendState::NotSentYet { progress: None }));
    });

    // Sending fails, the mock server has no matching route
    assert_let!(Some(VectorDiff::Set { index: 0, value }) = timeline_stream.next().await);
    assert_matches!(value.send_state(), Some(EventSendState::SendingFailed { .. }));

    // Discard, assert the local echo is found
    assert!(handle.abort().await.unwrap());

    // Observable local echo being removed
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 0 }));
}
