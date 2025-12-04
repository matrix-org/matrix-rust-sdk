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

use std::ops::Not as _;

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{
    room::Receipts,
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
};
use matrix_sdk_test::{
    ALICE, BOB, CAROL, JoinedRoomBuilder, RoomAccountDataTestEvent, async_test,
    event_factory::EventFactory,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineFocus, TimelineReadReceiptTracking};
use ruma::{
    MilliSecondsSinceUnixEpoch,
    api::client::receipt::create_receipt::v3::ReceiptType as CreateReceiptType,
    event_id,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, RoomAccountDataEventType,
        receipt::{ReceiptThread, ReceiptType as EventReceiptType},
        room::message::{MessageType, RoomMessageEventContent, SyncRoomMessageEvent},
    },
    owned_event_id, room_id,
    room_version_rules::RoomVersionRules,
    uint, user_id,
};
use serde_json::json;
use stream_assert::{assert_pending, assert_ready};
use tokio::task::yield_now;

fn filter_notice(ev: &AnySyncTimelineEvent, _rules: &RoomVersionRules) -> bool {
    match ev {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(msg),
        )) => !matches!(msg.content.msgtype, MessageType::Notice(_)),
        _ => true,
    }
}

#[async_test]
async fn test_read_receipts_updates() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    let second_event_id = event_id!("$e32037280er453l:localhost");
    let third_event_id = event_id!("$Sg2037280074GZr34:localhost");

    let timeline = room.timeline().await.unwrap();
    let (items, mut timeline_stream) = timeline.subscribe().await;
    let mut own_receipts_subscriber = timeline.subscribe_own_user_read_receipts_changed().await;

    assert!(items.is_empty());
    assert_pending!(own_receipts_subscriber);

    let own_user_id = client.user_id().unwrap();
    let alice = user_id!("@alice:localhost");
    let bob = user_id!("@bob:localhost");

    let own_receipt = timeline.latest_user_read_receipt(own_user_id).await;
    assert_matches!(own_receipt, None);
    let alice_receipt = timeline.latest_user_read_receipt(alice).await;
    assert_matches!(alice_receipt, None);
    let bob_receipt = timeline.latest_user_read_receipt(bob).await;
    assert_matches!(bob_receipt, None);

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("is dancing").sender(user_id!("@example:localhost")))
                .add_timeline_event(
                    f.text_msg("I'm dancing too").sender(alice).event_id(second_event_id),
                )
                .add_timeline_event(
                    f.text_msg("Viva la macarena!").sender(alice).event_id(third_event_id),
                ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 5);

    // We don't list the read receipt of our own user on events.
    assert_let!(VectorDiff::PushBack { value: first_item } = &timeline_updates[0]);
    let first_event = first_item.as_event().unwrap();
    assert!(first_event.read_receipts().is_empty());

    let (own_receipt_event_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_receipt_event_id, first_event.event_id().unwrap());

    assert_ready!(own_receipts_subscriber);
    assert_pending!(own_receipts_subscriber);

    // Implicit read receipt of @alice:localhost.
    assert_let!(VectorDiff::PushBack { value: second_item } = &timeline_updates[1]);
    let second_event = second_item.as_event().unwrap();
    assert_eq!(second_event.read_receipts().len(), 1);

    // Read receipt of @alice:localhost is moved to third event.
    assert_let!(VectorDiff::Set { index: 1, value: second_item } = &timeline_updates[2]);
    let second_event = second_item.as_event().unwrap();
    assert!(second_event.read_receipts().is_empty());

    assert_let!(VectorDiff::PushBack { value: third_item } = &timeline_updates[3]);
    let third_event = third_item.as_event().unwrap();
    assert_eq!(third_event.read_receipts().len(), 1);

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(alice).await.unwrap();
    assert_eq!(alice_receipt_event_id, third_event_id);

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[4]);
    assert!(date_divider.is_date_divider());

    // Read receipt on an unknown event is ignored.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(
                        event_id!("$unknowneventid"),
                        alice,
                        EventReceiptType::Read,
                        ReceiptThread::Unthreaded,
                    )
                    .into_event(),
            ),
        )
        .await;

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(alice).await.unwrap();
    assert_eq!(alice_receipt_event_id, third_event.event_id().unwrap());

    // Read receipt on older event is ignored.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(second_event_id, alice, EventReceiptType::Read, ReceiptThread::Unthreaded)
                    .into_event(),
            ),
        )
        .await;

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(alice).await.unwrap();
    assert_eq!(alice_receipt_event_id, third_event_id);

    // Read receipt on same event is ignored.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(third_event_id, alice, EventReceiptType::Read, ReceiptThread::Unthreaded)
                    .into_event(),
            ),
        )
        .await;

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(alice).await.unwrap();
    assert_eq!(alice_receipt_event_id, third_event_id);

    // New user with explicit threaded and unthreaded read receipts.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(second_event_id, bob, EventReceiptType::Read, ReceiptThread::Unthreaded)
                    .add(third_event_id, bob, EventReceiptType::Read, ReceiptThread::Main)
                    .into_event(),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::Set { index: 3, value: third_item } = &timeline_updates[0]);
    let third_event = third_item.as_event().unwrap();
    assert_eq!(third_event.read_receipts().len(), 2);

    let (bob_receipt_event_id, _) = timeline.latest_user_read_receipt(bob).await.unwrap();
    assert_eq!(bob_receipt_event_id, third_event_id);

    assert_pending!(own_receipts_subscriber);

    // Private read receipt is updated.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(
                        second_event_id,
                        own_user_id,
                        EventReceiptType::ReadPrivate,
                        ReceiptThread::Unthreaded,
                    )
                    .into_event(),
            ),
        )
        .await;

    let (own_user_receipt_event_id, _) =
        timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_user_receipt_event_id, second_event_id);

    assert_ready!(own_receipts_subscriber);
    assert_pending!(own_receipts_subscriber);
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_read_receipts_updates_on_filtered_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    let own_user_id = client.user_id().unwrap();

    let event_a_id = event_id!("$152037280074GZeOm:localhost");
    let event_b_id = event_id!("$e32037280er453l:localhost");
    let event_c_id = event_id!("$Sg2037280074GZr34:localhost");

    let timeline = room.timeline_builder().event_filter(filter_notice).build().await.unwrap();
    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert!(items.is_empty());

    let own_receipt = timeline.latest_user_read_receipt(own_user_id).await;
    assert_matches!(own_receipt, None);
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await;
    assert_matches!(own_receipt_timeline_event, None);
    let alice_receipt = timeline.latest_user_read_receipt(*ALICE).await;
    assert_matches!(alice_receipt, None);
    let alice_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*ALICE).await;
    assert_matches!(alice_receipt_timeline_event, None);
    let bob_receipt = timeline.latest_user_read_receipt(*BOB).await;
    assert_matches!(bob_receipt, None);
    let bob_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*BOB).await;
    assert_matches!(bob_receipt_timeline_event, None);

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                // Event A
                .add_timeline_event(
                    f.text_msg("is dancing").sender(own_user_id).event_id(event_a_id),
                )
                // Event B
                .add_timeline_event(f.notice("I'm dancing too").sender(*BOB).event_id(event_b_id))
                // Event C
                .add_timeline_event(
                    f.text_msg("Viva la macarena!").sender(*ALICE).event_id(event_c_id),
                ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 4);

    // We don't list the read receipt of our own user on events.
    assert_let!(VectorDiff::PushBack { value: item_a } = &timeline_updates[0]);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let (own_receipt_event_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_receipt_event_id, event_a_id);
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await.unwrap();
    assert_eq!(own_receipt_timeline_event, event_a_id);

    // Implicit read receipt of @bob:localhost.
    assert_let!(VectorDiff::Set { index: 0, value: item_a } = &timeline_updates[1]);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);

    // Real receipt is on event B.
    let (bob_receipt_event_id, _) = timeline.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(bob_receipt_event_id, event_b_id);
    // Visible receipt is on event A.
    let bob_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*BOB).await.unwrap();
    assert_eq!(bob_receipt_timeline_event, event_a.event_id().unwrap());

    // Implicit read receipt of @alice:localhost.
    assert_let!(VectorDiff::PushBack { value: item_c } = &timeline_updates[2]);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(alice_receipt_event_id, event_c_id);
    let alice_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*ALICE).await.unwrap();
    assert_eq!(alice_receipt_timeline_event, event_c_id);

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[3]);
    assert!(date_divider.is_date_divider());

    // Read receipt on filtered event.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(event_b_id, own_user_id, EventReceiptType::Read, ReceiptThread::Unthreaded)
                    .into_event(),
            ),
        )
        .await;

    // Real receipt changed to event B.
    let (own_receipt_event_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_receipt_event_id, event_b_id);
    // Visible receipt is still on event A.
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await.unwrap();
    assert_eq!(own_receipt_timeline_event, event_a.event_id().unwrap());

    // Update with explicit read receipt.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(event_c_id, *BOB, EventReceiptType::Read, ReceiptThread::Unthreaded)
                    .into_event(),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::Set { index: 1, value: item_a } = &timeline_updates[0]);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    assert_let!(VectorDiff::Set { index: 2, value: item_c } = &timeline_updates[1]);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 2);

    // Both real and visible receipts are now on event C.
    let (bob_receipt_event_id, _) = timeline.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(bob_receipt_event_id, event_c_id);
    let bob_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*BOB).await.unwrap();
    assert_eq!(bob_receipt_timeline_event, event_c_id);

    // Private read receipt is updated.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(
                        event_c_id,
                        own_user_id,
                        EventReceiptType::ReadPrivate,
                        ReceiptThread::Unthreaded,
                    )
                    .into_event(),
            ),
        )
        .await;

    // Both real and visible receipts are now on event C.
    let (own_user_receipt_event_id, _) =
        timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_user_receipt_event_id, event_c_id);
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await.unwrap();
    assert_eq!(own_receipt_timeline_event, event_c_id);
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_read_receipts_updates_on_message_like_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    let own_user_id = client.user_id().unwrap();

    let event_a_id = event_id!("$152037280074GZeOm:localhost");
    let event_b_id = event_id!("$e32037280er453l:localhost");
    let event_c_id = event_id!("$Sg2037280074GZr34:localhost");

    let timeline = room
        .timeline_builder()
        .track_read_marker_and_receipts(TimelineReadReceiptTracking::MessageLikeEvents)
        .build()
        .await
        .unwrap();
    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert!(items.is_empty());

    let own_receipt = timeline.latest_user_read_receipt(own_user_id).await;
    assert_matches!(own_receipt, None);
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await;
    assert_matches!(own_receipt_timeline_event, None);
    let alice_receipt = timeline.latest_user_read_receipt(*ALICE).await;
    assert_matches!(alice_receipt, None);
    let alice_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*ALICE).await;
    assert_matches!(alice_receipt_timeline_event, None);
    let bob_receipt = timeline.latest_user_read_receipt(*BOB).await;
    assert_matches!(bob_receipt, None);
    let bob_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*BOB).await;
    assert_matches!(bob_receipt_timeline_event, None);

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                // Event A
                .add_timeline_event(
                    f.text_msg("is dancing").sender(own_user_id).event_id(event_a_id),
                )
                // Event B
                .add_timeline_event(f.room_name("Party Room").sender(*BOB).event_id(event_b_id))
                // Event C
                .add_timeline_event(
                    f.text_msg("Viva la macarena!").sender(*ALICE).event_id(event_c_id),
                ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 5);

    // We don't list the read receipt of our own user on events.
    assert_let!(VectorDiff::PushBack { value: item_a } = &timeline_updates[0]);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let (own_receipt_event_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_receipt_event_id, event_a_id);
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await.unwrap();
    assert_eq!(own_receipt_timeline_event, event_a_id);

    // Implicit read receipt of @bob:localhost.
    assert_let!(VectorDiff::Set { index: 0, value: item_a } = &timeline_updates[1]);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);

    assert_let!(VectorDiff::PushBack { value: item_b } = &timeline_updates[2]);
    let event_b = item_b.as_event().unwrap();
    assert_eq!(event_b.read_receipts().len(), 0);

    // Real receipt is on event B.
    let (bob_receipt_event_id, _) = timeline.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(bob_receipt_event_id, event_b_id);
    // Visible receipt is on event A.
    let bob_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*BOB).await.unwrap();
    assert_eq!(bob_receipt_timeline_event, event_a.event_id().unwrap());

    // Implicit read receipt of @alice:localhost.
    assert_let!(VectorDiff::PushBack { value: item_c } = &timeline_updates[3]);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);

    let (alice_receipt_event_id, _) = timeline.latest_user_read_receipt(*ALICE).await.unwrap();
    assert_eq!(alice_receipt_event_id, event_c_id);
    let alice_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*ALICE).await.unwrap();
    assert_eq!(alice_receipt_timeline_event, event_c_id);

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[4]);
    assert!(date_divider.is_date_divider());

    // Read receipt on filtered event.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(event_b_id, own_user_id, EventReceiptType::Read, ReceiptThread::Unthreaded)
                    .into_event(),
            ),
        )
        .await;

    // Real receipt changed to event B.
    let (own_receipt_event_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_receipt_event_id, event_b_id);
    // Visible receipt is still on event A.
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await.unwrap();
    assert_eq!(own_receipt_timeline_event, event_a.event_id().unwrap());

    // Update with explicit read receipt.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(event_c_id, *BOB, EventReceiptType::Read, ReceiptThread::Unthreaded)
                    .into_event(),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::Set { index: 1, value: item_a } = &timeline_updates[0]);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    assert_let!(VectorDiff::Set { index: 3, value: item_c } = &timeline_updates[1]);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 2);

    // Both real and visible receipts are now on event C.
    let (bob_receipt_event_id, _) = timeline.latest_user_read_receipt(*BOB).await.unwrap();
    assert_eq!(bob_receipt_event_id, event_c_id);
    let bob_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(*BOB).await.unwrap();
    assert_eq!(bob_receipt_timeline_event, event_c_id);

    // Private read receipt is updated.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(
                        event_c_id,
                        own_user_id,
                        EventReceiptType::ReadPrivate,
                        ReceiptThread::Unthreaded,
                    )
                    .into_event(),
            ),
        )
        .await;

    // Both real and visible receipts are now on event C.
    let (own_user_receipt_event_id, _) =
        timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(own_user_receipt_event_id, event_c_id);
    let own_receipt_timeline_event =
        timeline.latest_user_read_receipt_timeline_event_id(own_user_id).await.unwrap();
    assert_eq!(own_receipt_timeline_event, event_c_id);
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_send_single_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: false })
        .build()
        .await
        .unwrap();

    // Unknown receipts are sent.
    let first_receipts_event_id = event_id!("$first_receipts_event_id");

    let mock_post_receipt_guards = (
        server
            .mock_send_receipt(CreateReceiptType::Read)
            .ok()
            .expect(1)
            .named("Public read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::ReadPrivate)
            .ok()
            .expect(1)
            .named("Private read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::FullyRead)
            .ok()
            .expect(1)
            .named("Fully-read marker")
            .mount_as_scoped()
            .await,
    );

    timeline
        .send_single_receipt(CreateReceiptType::Read, first_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::ReadPrivate, first_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::FullyRead, first_receipts_event_id.to_owned())
        .await
        .unwrap();

    drop(mock_post_receipt_guards);

    // Unchanged receipts are not sent.
    let f = EventFactory::new();
    let own_user_id = client.user_id().unwrap();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_receipt(
                    f.read_receipts()
                        .add(
                            first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::ReadPrivate,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                )
                .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "content": {
                        "event_id": first_receipts_event_id,
                    },
                    "type": "m.fully_read",
                }))),
        )
        .await;

    timeline
        .send_single_receipt(CreateReceiptType::Read, first_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::ReadPrivate, first_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::FullyRead, first_receipts_event_id.to_owned())
        .await
        .unwrap();

    // Receipts with unknown previous receipts are always sent.
    let second_receipts_event_id = event_id!("$second_receipts_event_id");

    let mock_post_receipt_guards = (
        server
            .mock_send_receipt(CreateReceiptType::Read)
            .ok()
            .expect(1)
            .named("Public read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::ReadPrivate)
            .ok()
            .expect(1)
            .named("Private read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::FullyRead)
            .ok()
            .expect(1)
            .named("Fully-read marker")
            .mount_as_scoped()
            .await,
    );

    timeline
        .send_single_receipt(CreateReceiptType::Read, second_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::ReadPrivate, second_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::FullyRead, second_receipts_event_id.to_owned())
        .await
        .unwrap();

    drop(mock_post_receipt_guards);

    // Newer receipts in the timeline are sent.
    let third_receipts_event_id = event_id!("$third_receipts_event_id");

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("I'm User A")
                        .sender(user_id!("@user_a:example.org"))
                        .event_id(second_receipts_event_id),
                )
                .add_timeline_event(
                    f.text_msg("I'm User B")
                        .sender(user_id!("@user_b:example.org"))
                        .event_id(third_receipts_event_id),
                )
                .add_receipt(
                    f.read_receipts()
                        .add(
                            second_receipts_event_id,
                            own_user_id,
                            EventReceiptType::ReadPrivate,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            second_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                )
                .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "content": {
                        "event_id": second_receipts_event_id,
                    },
                    "type": "m.fully_read",
                }))),
        )
        .await;

    let mock_post_receipt_guards = (
        server
            .mock_send_receipt(CreateReceiptType::Read)
            .ok()
            .expect(1)
            .named("Public read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::ReadPrivate)
            .ok()
            .expect(1)
            .named("Private read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::FullyRead)
            .ok()
            .expect(1)
            .named("Fully-read marker")
            .mount_as_scoped()
            .await,
    );

    timeline
        .send_single_receipt(CreateReceiptType::Read, third_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::ReadPrivate, third_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::FullyRead, third_receipts_event_id.to_owned())
        .await
        .unwrap();

    drop(mock_post_receipt_guards);

    // Older receipts in the timeline are not sent.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_receipt(
                    f.read_receipts()
                        .add(
                            third_receipts_event_id,
                            own_user_id,
                            EventReceiptType::ReadPrivate,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            third_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                )
                .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "content": {
                        "event_id": third_receipts_event_id,
                    },
                    "type": "m.fully_read",
                }))),
        )
        .await;

    timeline
        .send_single_receipt(CreateReceiptType::Read, second_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::ReadPrivate, second_receipts_event_id.to_owned())
        .await
        .unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::FullyRead, second_receipts_event_id.to_owned())
        .await
        .unwrap();
}

#[async_test]
async fn test_send_single_receipt_threaded() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    // A live timeline will use `main` as the post body `thread_id` parameter
    // value or no value at all for fully read markers.
    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: true })
        .build()
        .await
        .unwrap();

    let event_id = event_id!("$event_id");

    let mock_post_receipt_guards = (
        server
            .mock_send_receipt(CreateReceiptType::Read)
            .body_matches_partial_json(json!({
                "thread_id": "main",
            }))
            .ok()
            .expect(1)
            .named("Public read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::ReadPrivate)
            .body_matches_partial_json(json!({
                "thread_id": "main",
            }))
            .ok()
            .expect(1)
            .named("Private read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::FullyRead)
            .body_matches_partial_json(json!({}))
            .ok()
            .expect(1)
            .named("Fully-read marker")
            .mount_as_scoped()
            .await,
    );

    timeline.send_single_receipt(CreateReceiptType::Read, event_id.to_owned()).await.unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::ReadPrivate, event_id.to_owned())
        .await
        .unwrap();
    timeline.send_single_receipt(CreateReceiptType::FullyRead, event_id.to_owned()).await.unwrap();

    drop(mock_post_receipt_guards);

    let thread_root_event_id = event_id!("$thread_root");

    // A thread focused timeline will use the thread root event id as the post
    // body `thread_id` parameter value or no value at all for fully read
    // markers.

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id.to_owned() })
        .build()
        .await
        .unwrap();

    let event_id = event_id!("$event_id");

    let mock_post_receipt_guards = (
        server
            .mock_send_receipt(CreateReceiptType::Read)
            .body_matches_partial_json(json!({
                "thread_id": thread_root_event_id.to_string(),
            }))
            .ok()
            .expect(1)
            .named("Public read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::ReadPrivate)
            .body_matches_partial_json(json!({
                "thread_id": thread_root_event_id.to_string(),
            }))
            .ok()
            .expect(1)
            .named("Private read receipt")
            .mount_as_scoped()
            .await,
        server
            .mock_send_receipt(CreateReceiptType::FullyRead)
            .body_json(json!({}))
            .ok()
            .expect(1)
            .named("Fully-read marker")
            .mount_as_scoped()
            .await,
    );

    timeline.send_single_receipt(CreateReceiptType::Read, event_id.to_owned()).await.unwrap();
    timeline
        .send_single_receipt(CreateReceiptType::ReadPrivate, event_id.to_owned())
        .await
        .unwrap();
    timeline.send_single_receipt(CreateReceiptType::FullyRead, event_id.to_owned()).await.unwrap();

    drop(mock_post_receipt_guards);
}

#[async_test]
async fn test_send_single_receipt_with_unread_flag() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let own_user_id = client.user_id().unwrap();

    let first_receipts_event_id = owned_event_id!("$first_receipts_event_id");
    let second_receipts_event_id = owned_event_id!("$second_receipts_event_id");

    // Initial sync with our test room, with read receipts and marked unread.
    let f = EventFactory::new();
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_receipt(
                    f.read_receipts()
                        .add(
                            &first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::ReadPrivate,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            &first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            &first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Main,
                        )
                        .into_event(),
                )
                .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "content": {
                        "event_id": first_receipts_event_id,
                    },
                    "type": "m.fully_read",
                })))
                .add_account_data(RoomAccountDataTestEvent::MarkedUnread),
        )
        .await;
    assert!(room.is_marked_unread());

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: false })
        .build()
        .await
        .unwrap();

    // Unchanged unthreaded receipts are not sent, but the unread flag is unset.
    {
        let _guard = server
            .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
            .ok()
            .expect(3)
            .mount_as_scoped()
            .await;

        timeline
            .send_single_receipt(CreateReceiptType::Read, first_receipts_event_id.clone())
            .await
            .unwrap();
        timeline
            .send_single_receipt(CreateReceiptType::ReadPrivate, first_receipts_event_id.clone())
            .await
            .unwrap();
        timeline
            .send_single_receipt(CreateReceiptType::FullyRead, first_receipts_event_id)
            .await
            .unwrap();
    }

    // Unthreaded receipts on unknown events are set and the unread flag is unset.
    {
        let _guards = (
            server
                .mock_send_receipt(CreateReceiptType::Read)
                .ok()
                .expect(1)
                .named("Public read receipt")
                .mount_as_scoped()
                .await,
            server
                .mock_send_receipt(CreateReceiptType::ReadPrivate)
                .ok()
                .expect(1)
                .named("Private read receipt")
                .mount_as_scoped()
                .await,
            server
                .mock_send_receipt(CreateReceiptType::FullyRead)
                .ok()
                .expect(1)
                .named("Fully-read marker")
                .mount_as_scoped()
                .await,
            server
                .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
                .ok()
                .expect(3)
                .mount_as_scoped()
                .await,
        );

        timeline
            .send_single_receipt(CreateReceiptType::Read, second_receipts_event_id.clone())
            .await
            .unwrap();
        timeline
            .send_single_receipt(CreateReceiptType::ReadPrivate, second_receipts_event_id.clone())
            .await
            .unwrap();
        timeline
            .send_single_receipt(CreateReceiptType::FullyRead, second_receipts_event_id.clone())
            .await
            .unwrap();
    }

    // Threaded receipt with unknown previous receipt is sent, but the unread flag
    // is not unset.
    {
        let _guard = server
            .mock_send_receipt(CreateReceiptType::Read)
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;

        room.send_single_receipt(
            CreateReceiptType::Read,
            ReceiptThread::Main,
            second_receipts_event_id,
        )
        .await
        .unwrap();
    }
}

#[async_test]
async fn test_mark_as_read() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: false })
        .build()
        .await
        .unwrap();

    let original_event_id = event_id!("$original_event_id");
    let reaction_event_id = event_id!("$reaction_event_id");

    // When I receive an event with a reaction on it,
    let f = EventFactory::new();
    let own_user_id = client.user_id().unwrap();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("I like big Rust and I cannot lie")
                        .sender(user_id!("@sir-axalot:example.org"))
                        .event_id(original_event_id),
                )
                .add_receipt(
                    f.read_receipts()
                        .add(
                            original_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                )
                .add_timeline_event(
                    f.reaction(original_event_id, "🔥🔥🔥")
                        .sender(user_id!("@prime-minirusta:example.org"))
                        .event_id(reaction_event_id),
                ),
        )
        .await;

    // And I try to mark the latest event related to a timeline item as read,
    let latest_event_id = original_event_id;

    let has_sent = timeline
        .send_single_receipt(CreateReceiptType::Read, latest_event_id.to_owned())
        .await
        .unwrap();

    // Then no request is actually sent, because the event forming the timeline item
    // (the message) is known as read.
    assert!(has_sent.not());

    server.mock_send_receipt(CreateReceiptType::Read).ok().mock_once().mount().await;

    // But when I mark the room as read by sending a read receipt to the latest
    // event,
    let has_sent = timeline.mark_as_read(CreateReceiptType::Read).await.unwrap();

    // It works.
    assert!(has_sent);
}

#[async_test]
async fn test_mark_as_read_with_unread_flag() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Initial sync with our test room, marked unread.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_account_data(RoomAccountDataTestEvent::MarkedUnread),
        )
        .await;
    assert!(room.is_marked_unread());

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();

    let original_event_id = event_id!("$original_event_id");
    let reaction_event_id = event_id!("$reaction_event_id");

    // When I receive an event with a reaction on it,
    let f = EventFactory::new();
    let own_user_id = client.user_id().unwrap();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(
                    f.text_msg("I like big Rust and I cannot lie")
                        .sender(user_id!("@sir-axalot:example.org"))
                        .event_id(original_event_id),
                )
                .add_receipt(
                    f.read_receipts()
                        .add(
                            original_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                )
                .add_timeline_event(
                    f.reaction(original_event_id, "🔥🔥🔥")
                        .sender(user_id!("@prime-minirusta:example.org"))
                        .event_id(reaction_event_id),
                ),
        )
        .await;

    {
        let _send_receipt_guard = server
            .mock_send_receipt(CreateReceiptType::Read)
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;
        let _set_room_account_data_guard = server
            .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;

        // When I mark the room as read by sending a read receipt to the latest event,
        let has_sent = timeline.mark_as_read(CreateReceiptType::Read).await.unwrap();

        // The receipt is sent and the unread flag was unset.
        assert!(has_sent);
    }

    // Mock receiving the read receipt only.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_receipt(
                f.read_receipts()
                    .add(
                        reaction_event_id,
                        own_user_id,
                        EventReceiptType::Read,
                        ReceiptThread::Unthreaded,
                    )
                    .into_event(),
            ),
        )
        .await;

    {
        let _set_room_account_data_guard = server
            .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;

        // When I mark the room as read by sending a read receipt to the latest event,
        let has_sent = timeline.mark_as_read(CreateReceiptType::Read).await.unwrap();

        // The receipt is not sent but the unread flag was unset.
        assert!(!has_sent);
    }
}

#[async_test]
async fn test_send_multiple_receipts() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();

    // Unknown receipts are sent.
    let first_receipts_event_id = event_id!("$first_receipts_event_id");
    let first_receipts = Receipts::new()
        .fully_read_marker(Some(first_receipts_event_id.to_owned()))
        .public_read_receipt(Some(first_receipts_event_id.to_owned()))
        .private_read_receipt(Some(first_receipts_event_id.to_owned()));

    let guard = server.mock_send_read_markers().ok().expect(1).mount_as_scoped().await;

    timeline.send_multiple_receipts(first_receipts.clone()).await.unwrap();

    drop(guard);

    // Unchanged receipts are not sent.
    let f = EventFactory::new();
    let own_user_id = client.user_id().unwrap();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_receipt(
                    f.read_receipts()
                        .add(
                            first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::ReadPrivate,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                )
                .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "content": {
                        "event_id": first_receipts_event_id,
                    },
                    "type": "m.fully_read",
                }))),
        )
        .await;

    timeline.send_multiple_receipts(first_receipts).await.unwrap();

    // Receipts with unknown previous receipts are always sent.
    let second_receipts_event_id = event_id!("$second_receipts_event_id");
    let second_receipts = Receipts::new()
        .fully_read_marker(Some(second_receipts_event_id.to_owned()))
        .public_read_receipt(Some(second_receipts_event_id.to_owned()))
        .private_read_receipt(Some(second_receipts_event_id.to_owned()));

    let guard = server.mock_send_read_markers().ok().expect(1).mount_as_scoped().await;

    timeline.send_multiple_receipts(second_receipts.clone()).await.unwrap();
    drop(guard);

    // Newer receipts in the timeline are sent.
    let third_receipts_event_id = event_id!("$third_receipts_event_id");
    let third_receipts = Receipts::new()
        .fully_read_marker(Some(third_receipts_event_id.to_owned()))
        .public_read_receipt(Some(third_receipts_event_id.to_owned()))
        .private_read_receipt(Some(third_receipts_event_id.to_owned()));

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("I'm User A")
                        .sender(user_id!("@user_a:example.org"))
                        .event_id(second_receipts_event_id),
                )
                .add_timeline_event(
                    f.text_msg("I'm User B")
                        .sender(user_id!("@user_b:example.org"))
                        .event_id(third_receipts_event_id),
                )
                .add_receipt(
                    f.read_receipts()
                        .add(
                            second_receipts_event_id,
                            own_user_id,
                            EventReceiptType::ReadPrivate,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            second_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                )
                .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "content": {
                        "event_id": second_receipts_event_id,
                    },
                    "type": "m.fully_read",
                }))),
        )
        .await;

    let guard = server.mock_send_read_markers().ok().expect(1).mount_as_scoped().await;

    timeline.send_multiple_receipts(third_receipts.clone()).await.unwrap();
    drop(guard);

    // Older receipts in the timeline are not sent.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_receipt(
                    f.read_receipts()
                        .add(
                            third_receipts_event_id,
                            own_user_id,
                            EventReceiptType::ReadPrivate,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            third_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                )
                .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "content": {
                        "event_id": third_receipts_event_id,
                    },
                    "type": "m.fully_read",
                }))),
        )
        .await;

    timeline.send_multiple_receipts(second_receipts.clone()).await.unwrap();
}

#[async_test]
async fn test_send_multiple_receipts_with_unread_flag() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let own_user_id = client.user_id().unwrap();

    let first_receipts_event_id = owned_event_id!("$first_receipts_event_id");
    let second_receipts_event_id = owned_event_id!("$second_receipts_event_id");

    // Initial sync with our test room, with read receipts and marked unread.
    let f = EventFactory::new();
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_receipt(
                    f.read_receipts()
                        .add(
                            &first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::ReadPrivate,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            &first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .add(
                            &first_receipts_event_id,
                            own_user_id,
                            EventReceiptType::Read,
                            ReceiptThread::Main,
                        )
                        .into_event(),
                )
                .add_account_data(RoomAccountDataTestEvent::Custom(json!({
                    "content": {
                        "event_id": first_receipts_event_id,
                    },
                    "type": "m.fully_read",
                })))
                .add_account_data(RoomAccountDataTestEvent::MarkedUnread),
        )
        .await;
    assert!(room.is_marked_unread());

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();

    // Unchanged receipts are not sent, but the unread flag is unset.
    {
        let _guard = server
            .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;

        let first_receipts = Receipts::new()
            .fully_read_marker(Some(first_receipts_event_id.clone()))
            .public_read_receipt(Some(first_receipts_event_id.clone()))
            .private_read_receipt(Some(first_receipts_event_id));
        timeline.send_multiple_receipts(first_receipts).await.unwrap();
    }

    // Receipts with unknown previous receipts are always sent, and the unread flag
    // is unset.
    {
        let _read_markers_guard =
            server.mock_send_read_markers().ok().expect(1).mount_as_scoped().await;
        let _marked_unread_guard = server
            .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;

        let second_receipts = Receipts::new()
            .fully_read_marker(Some(second_receipts_event_id.clone()))
            .public_read_receipt(Some(second_receipts_event_id.clone()))
            .private_read_receipt(Some(second_receipts_event_id));
        timeline.send_multiple_receipts(second_receipts.clone()).await.unwrap();
    }
}

#[async_test]
async fn test_latest_user_read_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();

    let (items, _) = timeline.subscribe().await;

    assert!(items.is_empty());

    let own_user_id = client.user_id().unwrap();
    let user_receipt = timeline.latest_user_read_receipt(own_user_id).await;
    assert_matches!(user_receipt, None);

    let event_a_id = event_id!("$event_a");
    let event_b_id = event_id!("$event_b");
    let event_c_id = event_id!("$event_c");
    let event_d_id = event_id!("$event_d");
    let event_e_id = event_id!("$event_e");

    // Only private receipt.
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add_with_timestamp(
                        event_a_id,
                        own_user_id,
                        EventReceiptType::ReadPrivate,
                        ReceiptThread::Unthreaded,
                        None,
                    )
                    .into_event(),
            ),
        )
        .await;

    let (user_receipt_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(user_receipt_id, event_a_id);

    // Private and public receipts without timestamp should return private
    // receipt.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add_with_timestamp(
                        event_b_id,
                        own_user_id,
                        EventReceiptType::Read,
                        ReceiptThread::Unthreaded,
                        None,
                    )
                    .into_event(),
            ),
        )
        .await;

    let (user_receipt_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(user_receipt_id, event_a_id);

    // Public receipt with bigger timestamp.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add_with_timestamp(
                        event_c_id,
                        own_user_id,
                        EventReceiptType::ReadPrivate,
                        ReceiptThread::Unthreaded,
                        Some(MilliSecondsSinceUnixEpoch(uint!(1))),
                    )
                    .add_with_timestamp(
                        event_d_id,
                        own_user_id,
                        EventReceiptType::Read,
                        ReceiptThread::Unthreaded,
                        Some(MilliSecondsSinceUnixEpoch(uint!(10))),
                    )
                    .into_event(),
            ),
        )
        .await;

    let (user_receipt_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(user_receipt_id, event_d_id);

    // Private receipt with bigger timestamp.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add_with_timestamp(
                        event_e_id,
                        own_user_id,
                        EventReceiptType::ReadPrivate,
                        ReceiptThread::Unthreaded,
                        Some(MilliSecondsSinceUnixEpoch(uint!(100))),
                    )
                    .into_event(),
            ),
        )
        .await;

    let (user_receipt_id, _) = timeline.latest_user_read_receipt(own_user_id).await.unwrap();
    assert_eq!(user_receipt_id, event_e_id);
}

#[async_test]
async fn test_no_duplicate_receipt_after_backpagination() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!a98sd12bjh:example.org");

    // We want the following final state in the room:
    // - received from back-pagination:
    //  - $1: an event from Alice
    //  - $2: an event from Bob
    // - received from sync:
    //  - $3: an hidden event sent by Alice, with a read receipt from Carol
    //
    // As a result, since $3 is *after* the two others, Alice's implicit read
    // receipt and Carol's receipt on the edit event should be placed onto the
    // most recent rendered event, that is, $2.

    let eid1 = event_id!("$1_backpaginated_oldest");
    let eid2 = event_id!("$2_backpaginated_newest");
    let eid3 = event_id!("$3_sync_event");

    let f = EventFactory::new().room(room_id);

    // Alice sends an edit via sync.
    let ev3 = f
        .text_msg("* I am Alice.")
        .edit(eid1, RoomMessageEventContent::text_plain("I am Alice.").into())
        .sender(*ALICE)
        .event_id(eid3)
        .into_raw_sync();

    // Carol has a read receipt on the edit.
    let read_receipt_event = f
        .read_receipts()
        .add(eid3, *CAROL, ruma::events::receipt::ReceiptType::Read, ReceiptThread::Unthreaded)
        .into_event();

    let prev_batch_token = "prev-batch-token";

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(ev3)
                .add_receipt(read_receipt_event.room(room_id))
                .set_timeline_limited()
                .set_timeline_prev_batch(prev_batch_token),
        )
        .await;

    let timeline = room.timeline().await.unwrap();

    server
        .mock_room_messages()
        .match_from(prev_batch_token)
        .ok(RoomMessagesResponseTemplate::default().events(vec![
            // In reverse order!
            f.text_msg("I am Bob.").sender(*BOB).event_id(eid2),
            f.text_msg("I am the destroyer of worlds.").sender(*ALICE).event_id(eid1),
        ]))
        .mock_once()
        .mount()
        .await;

    let reached_start = timeline.paginate_backwards(42).await.unwrap();
    assert!(reached_start);

    yield_now().await;

    // Check that the receipts are at the correct place.
    let timeline_items = timeline.items().await;
    assert_eq!(timeline_items.len(), 4);

    assert!(timeline_items[0].is_timeline_start());
    assert!(timeline_items[1].is_date_divider());

    {
        let event1 = timeline_items[2].as_event().unwrap();
        // Sanity check: this is the edited event from Alice.
        assert_eq!(event1.event_id().unwrap(), eid1);

        let receipts = &event1.read_receipts();

        // Carol has explicitly seen ev3, which is after Bob's event, so there shouldn't
        // be a receipt for them here.
        assert!(receipts.get(*CAROL).is_none());

        // Alice has seen this event, being the sender; but Alice has also sent an edit
        // after Bob's message, so Alice must not have a read receipt here.
        assert!(receipts.get(*ALICE).is_none());

        // And Bob has seen the original, but posted something after it, so no receipt
        // for Bob either.
        assert!(receipts.get(*BOB).is_none());

        // In other words, no receipts here.
        assert!(receipts.is_empty());
    }

    {
        let event2 = timeline_items[3].as_event().unwrap();

        // Sanity check: this is Bob's event.
        assert_eq!(event2.event_id().unwrap(), eid2);

        let receipts = &event2.read_receipts();
        // Bob's event should hold *all* the receipts:
        assert_eq!(receipts.len(), 3);
        receipts.get(*ALICE).unwrap();
        receipts.get(*BOB).unwrap();
        receipts.get(*CAROL).unwrap();
    }
}
