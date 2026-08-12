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
use futures_util::{FutureExt as _, StreamExt};
use matrix_sdk::{assert_let_timeout, executor::spawn, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{EventSendState, RoomExt};
use ruma::{
    EventId, event_id,
    events::room::message::{MessageType, RoomMessageEventContent},
    room_id, user_id,
};
use stream_assert::{assert_next_matches, assert_pending};
use tokio::task::yield_now;

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

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
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

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
    assert_eq!(timeline_updates.len(), 1);

    // The `EventSendState` has been updated.
    assert_let!(VectorDiff::Set { index: 1, value: sent_confirmation } = &timeline_updates[0]);
    let item = sent_confirmation.as_event().unwrap();
    assert_matches!(item.send_state(), Some(EventSendState::Sent { .. }));
    assert_eq!(item.event_id(), Some(event_id));

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
    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
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
    // indicating something's wrong on the client side. The send queue uses
    // `short_retry()` (3 retries) with 500ms minimum exponential backoff, so
    // this can take up to ~1.5s before the failure is surfaced.
    assert_let_timeout!(
        Duration::from_secs(5),
        Some(VectorDiff::Set { index: 0, value: item }) = timeline_stream.next()
    );
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
    assert_let_timeout!(Some(VectorDiff::Set { index: 0, value }) = timeline_stream.next());
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
        // Not great to use a timer for this, but it's what wiremock gives us right now.
        // Ideally we'd wait on a channel to produce a value or sth. like that, but
        // wiremock doesn't allow to handle multiple queries at the same time.
        .ok_with_delay(event_id, Duration::from_millis(500))
        .mount()
        .await;

    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await.unwrap();

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
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
                    .sender(client.user_id().unwrap())
                    .event_id(event_id)
                    .server_ts(123456),
            ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
    assert_eq!(timeline_updates.len(), 2);

    // Timeline: [remote-echo, date-divider, local echo]
    assert_let!(VectorDiff::PushFront { value: remote_echo } = &timeline_updates[0]);
    let item = remote_echo.as_event().unwrap();
    assert_eq!(item.event_id(), Some(event_id));

    // Timeline: [date-divider, remote-echo, date-divider, local echo]
    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    // The mock server has a 500ms delay, so we need more than 100ms here.
    assert_let_timeout!(Duration::from_secs(2), Some(timeline_updates) = timeline_stream.next());
    assert_eq!(timeline_updates.len(), 4);

    // Local echo and its date divider are removed.
    // Timeline: [date-divider, remote-echo, date-divider]
    assert_let!(VectorDiff::Remove { index: 3 } = &timeline_updates[0]);

    // Timeline: [date-divider, remote-echo]
    assert_let!(VectorDiff::Remove { index: 2 } = &timeline_updates[1]);

    // The dedup false negative (the event received by sync has been sent by the
    // current user; see #6190 for details) re-delivers the event, but as it's
    // already in the loaded tail it's replaced in place rather than moved.
    // Timeline: [date-divider, remote-echo]
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[2]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(event_id));

    assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[3]);
    assert!(value.is_date_divider());

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
    assert_let_timeout!(Some(VectorDiff::Set { index: 0, value }) = timeline_stream.next());
    assert_matches!(value.send_state(), Some(EventSendState::SendingFailed { .. }));

    // Discard, assert the local echo is found
    assert!(handle.abort().await.unwrap());

    // Observable local echo being removed
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 0 }));
}

#[async_test]
async fn test_limited_gappy_sync_redelivering_own_sends_keeps_them_visible() {
    // Regression test for messages vanishing after being sent on a slow
    // connection: a limited sync with a new gap whose batch consists solely of
    // our own just-sent events (already eagerly inserted at the tail by the
    // send queue) must leave those events visible in the subscriber's view.
    //
    // The subscriber's lazy skip count must be engaged (i.e. the timeline
    // holds more than 20 items) to reproduce the original bug: the skip
    // stream's translation of the post-shrink `Clear` is where the view
    // diverged.
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!a98sd12bjh:example.org");
    let f = EventFactory::new();

    // Enough initial history that the subscriber's lazy skip count is
    // non-zero (more than 20 items), like a real app timeline.
    let mut initial_room_builder = JoinedRoomBuilder::new(room_id);
    for i in 0u32..30 {
        initial_room_builder = initial_room_builder.add_timeline_event(
            f.text_msg(format!("hello {i}"))
                .sender(user_id!("@bob:example.org"))
                .event_id(&EventId::parse(format!("$prior{i}")).unwrap())
                .server_ts(152038200 + u64::from(i)),
        );
    }
    let room = server.sync_room(&client, initial_room_builder).await;

    // A limited sync collapses the room to its last chunk, leaving the older
    // history behind a gap in the store: the shape a long-lived room takes
    // after any timeline-limited catch-up.
    let mut collapse_builder = JoinedRoomBuilder::new(room_id)
        .set_timeline_limited()
        .set_timeline_prev_batch("gap-1".to_owned());
    for i in 30u32..35 {
        collapse_builder = collapse_builder.add_timeline_event(
            f.text_msg(format!("hello {i}"))
                .sender(user_id!("@bob:example.org"))
                .event_id(&EventId::parse(format!("$prior{i}")).unwrap())
                .server_ts(152038200 + u64::from(i)),
        );
    }
    server.sync_room(&client, collapse_builder).await;

    server.mock_room_state_encryption().plain().mount().await;

    // Back-pagination fills the gap with the 30 older events (reverse
    // topological order), reaching the start of the timeline.
    server
        .mock_room_messages()
        .ok(matrix_sdk::test_utils::mocks::RoomMessagesResponseTemplate::default().events(
            (0u32..30)
                .rev()
                .map(|i| {
                    f.text_msg(format!("hello {i}"))
                        .room(room_id)
                        .sender(user_id!("@bob:example.org"))
                        .event_id(&EventId::parse(format!("$prior{i}")).unwrap())
                        .server_ts(152038200 + u64::from(i))
                })
                .collect::<Vec<_>>(),
        ))
        .mount()
        .await;

    let timeline = Arc::new(room.timeline().await.unwrap());

    // Load the history into the timeline, as the app does when opening the
    // room: this reloads the pre-gap history into memory, giving the linked
    // chunk the same multi-chunk shape as a real room.
    timeline.paginate_backwards(20).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Observe through the real subscriber (with its lazy skip count), as the
    // FFI layer does.
    let (mut observed, mut timeline_stream) = timeline.subscribe().await;

    let event_ids = [event_id!("$d"), event_id!("$e"), event_id!("$f"), event_id!("$g")];

    // Send four messages for real, so the timeline items go through the full
    // local echo -> Sent -> eager event cache insert lifecycle.
    for event_id in event_ids {
        server.mock_room_send().ok(event_id).mock_once().mount().await;
    }
    for body in ["d", "e", "f", "g"] {
        timeline.send(RoomMessageEventContent::text_plain(body).into()).await.unwrap();
    }

    // Wait until all four have been assigned their event ids, catching the
    // subscriber's view up along the way and capturing the transaction ids
    // for the sync re-delivery below.
    let mut txn_ids = std::collections::HashMap::new();
    for _ in 0u32..50 {
        while let Some(Some(diffs)) = timeline_stream.next().now_or_never() {
            for diff in diffs {
                // The local echoes only live for the blink of an eye, so
                // capture their transaction ids from the diffs themselves.
                diff.clone().map(|item| {
                    if let Some(event) = item.as_event() {
                        if let (Some(msg), Some(txn_id)) =
                            (event.content().as_message(), event.transaction_id())
                        {
                            txn_ids.insert(msg.body().to_owned(), txn_id.to_owned());
                        }
                    }
                    item
                });
                diff.apply(&mut observed);
            }
        }

        let sent = observed
            .iter()
            .filter(|item| {
                item.as_event()
                    .and_then(|ev| ev.event_id())
                    .is_some_and(|id| event_ids.contains(&id))
            })
            .count();
        if sent == 4 && txn_ids.len() == 4 {
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(txn_ids.len(), 4, "sends did not all settle: {txn_ids:?}");

    // The slow-network sync catches up: limited, with a new gap, and the
    // batch is exactly our four events again.
    let mut sync_room_builder = JoinedRoomBuilder::new(room_id)
        .set_timeline_limited()
        .set_timeline_prev_batch("prev-batch-token".to_owned());
    for (i, (body, event_id)) in ["d", "e", "f", "g"].iter().zip(event_ids).enumerate() {
        sync_room_builder = sync_room_builder.add_timeline_event(
            f.text_msg(*body)
                .sender(client.user_id().unwrap())
                .event_id(event_id)
                .server_ts(152038280 + i as u64)
                .unsigned_transaction_id(&txn_ids[*body]),
        );
    }
    server.sync_room(&client, sync_room_builder).await;

    // Let the timeline process everything, applying every subscriber batch.
    tokio::time::sleep(Duration::from_millis(300)).await;
    while let Some(Some(diffs)) = timeline_stream.next().now_or_never() {
        for diff in diffs {
            diff.apply(&mut observed);
        }
    }

    let visible_ids = observed
        .iter()
        .filter_map(|item| item.as_event().and_then(|ev| ev.event_id()).map(|id| id.to_string()))
        .collect::<Vec<_>>();

    // The subscriber's view must be a suffix of the real timeline: any stale
    // items or misplaced events mean the view has diverged.
    let raw_ids = timeline
        .items()
        .await
        .iter()
        .filter_map(|item| item.as_event().and_then(|ev| ev.event_id()).map(|id| id.to_string()))
        .collect::<Vec<_>>();

    assert!(
        raw_ids.ends_with(&visible_ids),
        "the subscriber view diverged from the timeline: view {visible_ids:?} vs timeline {raw_ids:?}"
    );

    // And the four re-delivered events must still be visible, in order, at
    // the tail.
    assert_eq!(
        visible_ids.iter().rev().take(4).rev().cloned().collect::<Vec<_>>(),
        event_ids.iter().map(|id| id.to_string()).collect::<Vec<_>>(),
        "own sent events are not the tail of the observed timeline: {visible_ids:?}"
    );
}
