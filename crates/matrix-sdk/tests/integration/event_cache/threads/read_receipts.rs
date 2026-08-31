// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! All the tests in this file follow the same pattern:
//!
//! - join a room with no events at first,
//! - then, start a listener to get notified when the read receipt state is
//!   updated,
//! - then, sync a batch of events (messages, receipts, etc.) that should
//!   trigger an update for the listener,
//! - then, run assertions on the thread unread counts.
//!
//! This avoids potential race conditions where a sync could be done, but the
//! processing by the event cache isn't, at the time we check the unread counts.

use std::time::Duration;

use matrix_sdk::{assert_let_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{
    event_id,
    events::{
        Mentions,
        receipt::{ReceiptThread, ReceiptType},
        room::member::MembershipState,
    },
    room_id,
};
use tokio::time::sleep;

/// Test that the unread count increases when new messages arrive and no read
/// receipt is known.
#[async_test]
async fn test_unread_count_new_message_no_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();

    assert!(thread_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("hello").in_thread(thread_id, thread_id).event_id(event_id!("$1")),
                )
                .add_timeline_event(
                    f.text_msg("world").in_thread(thread_id, thread_id).event_id(event_id!("$2")),
                ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // Both messages from Alice count as unread since there is no read receipt.
    assert_eq!(thread.num_unread_messages().await.unwrap(), 2);
}

/// Test that the unread count only includes messages after the last known read
/// receipt.
#[async_test]
async fn test_unread_count_new_message_with_known_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let own_user_id = client.user_id().unwrap();
    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("ev1").in_thread(thread_id, thread_id).event_id(event_id!("$1")),
                )
                .add_timeline_event(
                    f.text_msg("ev2").in_thread(thread_id, thread_id).event_id(event_id!("$2")),
                )
                .add_timeline_event(
                    f.text_msg("ev3").in_thread(thread_id, thread_id).event_id(event_id!("$3")),
                )
                // The current user has read up to ev1.
                .add_receipt(
                    f.read_receipts()
                        .add(
                            event_id!("$2"),
                            own_user_id,
                            ReceiptType::Read,
                            ReceiptThread::Thread(thread_id.to_owned()),
                        )
                        .into_event(),
                ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // Only ev3 (after the receipt) is unread.
    assert_eq!(thread.num_unread_messages().await.unwrap(), 1);
}

/// Test that a message sent by the current user creates an implicit read
/// receipt so that only messages after the user's own message count as unread.
#[async_test]
async fn test_unread_count_implicit_receipt_own_message() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);
    let own_user_id = client.user_id().unwrap();

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("ev1").in_thread(thread_id, thread_id).event_id(event_id!("$1")),
                )
                .add_timeline_event(
                    f.text_msg("ev2").in_thread(thread_id, thread_id).event_id(event_id!("$2")),
                )
                // The current user sends ev3: implicit read receipt up to here.
                .add_timeline_event(
                    f.text_msg("ev3")
                        .in_thread(thread_id, thread_id)
                        .sender(own_user_id)
                        .event_id(event_id!("$3")),
                )
                .add_timeline_event(
                    f.text_msg("ev4").in_thread(thread_id, thread_id).event_id(event_id!("$4")),
                )
                .add_timeline_event(
                    f.text_msg("ev5").in_thread(thread_id, thread_id).event_id(event_id!("$5")),
                ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // ev4 and ev5 (after our own ev3) are unread; ev1/ev2/ev3 are read via
    // implicit receipt.
    let read_receipts = thread.read_receipts().await.unwrap();
    assert_eq!(read_receipts.num_unread, 2);
    assert_eq!(read_receipts.latest_active.unwrap().event_id, event_id!("$3"));
}

/// Test that receiving only a new read receipt event (with no new messages)
/// decreases the unread count.
#[async_test]
async fn test_unread_count_receipt_only_no_new_message() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);
    let own_user_id = client.user_id().unwrap();

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    // First sync: three messages from Alice, no receipt.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("ev1").in_thread(thread_id, thread_id).event_id(event_id!("$1")),
                )
                .add_timeline_event(
                    f.text_msg("ev2").in_thread(thread_id, thread_id).event_id(event_id!("$2")),
                )
                .add_timeline_event(
                    f.text_msg("ev3").in_thread(thread_id, thread_id).event_id(event_id!("$3")),
                ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    assert_eq!(thread.num_unread_messages().await.unwrap(), 3);

    // Second sync: a read receipt for ev2 arrives, no new messages.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(
                        event_id!("$2"),
                        own_user_id,
                        ReceiptType::Read,
                        ReceiptThread::Thread(thread_id.to_owned()),
                    )
                    .into_event(),
            ),
        )
        .await;

    // Can't wait on `thread_updates` because there is no update “read receipt
    // update” for threads.
    sleep(Duration::from_millis(100)).await;

    // Only ev3 (after the receipt) is unread now.
    assert_eq!(thread.num_unread_messages().await.unwrap(), 1);
}

/// Test that a read receipt for an unknown event is stored as pending, and
/// resolves once that event arrives in a subsequent sync.
#[async_test]
async fn test_unread_count_pending_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);
    let own_user_id = client.user_id().unwrap();

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    // First sync: three messages from Alice plus a receipt for a future event.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("ev1").in_thread(thread_id, thread_id).event_id(event_id!("$1")),
                )
                .add_timeline_event(
                    f.text_msg("ev2").in_thread(thread_id, thread_id).event_id(event_id!("$2")),
                )
                .add_timeline_event(
                    f.text_msg("ev3").in_thread(thread_id, thread_id).event_id(event_id!("$3")),
                )
                // Receipt refers to $future, which isn't known yet.
                .add_receipt(
                    f.read_receipts()
                        .add(
                            event_id!("$future"),
                            own_user_id,
                            ReceiptType::Read,
                            ReceiptThread::Thread(thread_id.to_owned()),
                        )
                        .into_event(),
                ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // All three events are unread because the receipt target is unknown.
    let read_receipts = thread.read_receipts().await.unwrap();
    assert_eq!(read_receipts.num_unread, 3);
    // The receipt is stored as pending.
    assert!(read_receipts.pending.iter().any(|id| id == event_id!("$future")));
    assert_eq!(read_receipts.pending.len(), 1);

    // Second sync: $future arrives along with another message.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("the future event")
                        .in_thread(thread_id, thread_id)
                        .event_id(event_id!("$future")),
                )
                .add_timeline_event(
                    f.text_msg("ev4").in_thread(thread_id, thread_id).event_id(event_id!("$4")),
                ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // The pending receipt resolves: only ev4 (after $future) is unread.
    let read_receipts = thread.read_receipts().await.unwrap();
    assert_eq!(read_receipts.num_unread, 1);
    assert!(read_receipts.pending.is_empty());
}

/// Test that unread counts accumulate across multiple syncs when no read
/// receipt is updated.
#[async_test]
async fn test_unread_count_accumulates_across_syncs() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    // First sync: two messages.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("ev1").in_thread(thread_id, thread_id).event_id(event_id!("$1")),
                )
                .add_timeline_event(
                    f.text_msg("ev2").in_thread(thread_id, thread_id).event_id(event_id!("$2")),
                ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    assert_eq!(thread.num_unread_messages().await.unwrap(), 2);

    // Second sync: one more message, still no receipt.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("ev3").in_thread(thread_id, thread_id).event_id(event_id!("$3")),
            ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // Three messages are now unread in total.
    assert_eq!(thread.num_unread_messages().await.unwrap(), 3);
}

/// Test that state events (e.g. a membership change) in the timeline do not
/// increment the unread count.
#[async_test]
async fn test_state_event_does_not_increment_unread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id);

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();

    server
        .sync_room(
            &client,
            // Alice joining the room is a state event, not a message.
            JoinedRoomBuilder::new(room_id).add_timeline_state_bulk([f
                .member(*ALICE)
                .membership(MembershipState::Join)
                .event_id(event_id!("$1"))
                .into_raw_sync_state()]),
        )
        .await;

    sleep(Duration::from_millis(100)).await;

    assert_eq!(thread.num_unread_messages().await.unwrap(), 0);
}

/// Test that reactions don't count in the unread message count.
#[async_test]
async fn test_reaction_does_not_increment_unread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk([
                f.text_msg("hello")
                    .in_thread(thread_id, thread_id)
                    .event_id(event_id!("$1"))
                    .into_raw(),
                f.reaction(event_id!("$1"), "👍").event_id(event_id!("$2")).into(),
            ]),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // Only the text message counts as unread, the reaction doesn't.
    assert_eq!(thread.num_unread_messages().await.unwrap(), 1);
}

/// Test that messages with mentions increment the number of mentions.
#[async_test]
async fn test_mentions_increments_unread_mentions() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    // For mentions to be properly counted, we need to have a member event for the
    // current user.
    let member_event = f
        .member(client.user_id().unwrap())
        .membership(MembershipState::Join)
        .event_id(event_id!("$member"));

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("hello example")
                        .in_thread(thread_id, thread_id)
                        .event_id(event_id!("$1"))
                        .mentions(Mentions::with_user_ids([client.user_id().unwrap().to_owned()])),
                )
                .add_state_event(member_event),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // The message counts as unread and also increments the mentions count.
    assert_eq!(thread.num_unread_messages().await.unwrap(), 1);
    assert_eq!(thread.num_unread_mentions().await.unwrap(), 1);
}

/// Test that the unread count computation doesn't skip a more-recent active
/// receipt, when iterating over events.
#[async_test]
async fn test_compute_unread_counts_considers_active_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let own_user_id = client.user_id().unwrap();

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;

    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    // Starting with a room with 1 implicit receipt, then two messages from Alice,
    // and a receipt on Alice's first message $2,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_bulk([
                    f.text_msg("hello 1")
                        .in_thread(thread_id, thread_id)
                        .sender(own_user_id)
                        .event_id(event_id!("$1"))
                        .into_raw(),
                    f.text_msg("hello 2")
                        .in_thread(thread_id, thread_id)
                        .event_id(event_id!("$2"))
                        .into_raw(),
                    f.text_msg("hello 3")
                        .in_thread(thread_id, thread_id)
                        .event_id(event_id!("$3"))
                        .into_raw(),
                ])
                .add_receipt(
                    f.read_receipts()
                        .add(
                            event_id!("$2"),
                            own_user_id,
                            ReceiptType::Read,
                            ReceiptThread::Thread(thread_id.to_owned()),
                        )
                        .into_event(),
                ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // The message counts are properly updated (one new message unread after $2).
    assert_eq!(thread.num_unread_messages().await.unwrap(), 1);

    // Provided a sync with one new message from Alice in the same room.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("hello 4").in_thread(thread_id, thread_id).event_id(event_id!("$4")),
            ),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // The message counts are properly updated (two messages after $2).
    assert_eq!(thread.num_unread_messages().await.unwrap(), 2);
}

/// Test that the unread count gets updated when the sync update only contains
/// duplicate events and a new read receipt.
#[async_test]
async fn test_unread_counts_updated_after_duplicate_only_sync_response() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let own_user_id = client.user_id().unwrap();

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;

    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    // Starting with a message from Bob, and no read receipt,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk([
                f.text_msg("hello 1")
                    .in_thread(thread_id, thread_id)
                    .event_id(event_id!("$1"))
                    .into_raw(),
                f.text_msg("hello 2")
                    .in_thread(thread_id, thread_id)
                    .event_id(event_id!("$2"))
                    .into_raw(),
            ]),
        )
        .await;

    // We receive the thread update for the two new messages.
    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // Then, provided a sync with a single duplicated message sent by somebody else,
    // but a read receipt for the existing message $2,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("hello 2").in_thread(thread_id, thread_id).event_id(event_id!("$2")),
                )
                .add_receipt(
                    f.read_receipts()
                        .add(
                            event_id!("$2"),
                            own_user_id,
                            ReceiptType::Read,
                            ReceiptThread::Thread(thread_id.to_owned()),
                        )
                        .into_event(),
                ),
        )
        .await;

    // We don't get an update about the read receipt (because threads can't do
    // that), and we don't get an update about new event because it's been
    // deduplicated. So. Just wait a little bit :-].
    sleep(Duration::from_millis(100)).await;

    // The message counts are properly updated (zero new message unread after $2).
    assert_eq!(thread.num_unread_messages().await.unwrap(), 0);
}

/// Test that a read receipt saved in the state store but not marked as active
/// is selected for unread count computation.
#[async_test]
async fn test_read_receipt_from_store_used_as_latest_active() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let own_user_id = client.user_id().unwrap();
    let room_id = room_id!("!r");
    let thread_id = event_id!("$t");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    // Important test note: the read receipt must be in the state store *before* the
    // event cache is subscribed to, so that it's not marked as active at start.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(
                        event_id!("$2"),
                        own_user_id,
                        ReceiptType::Read,
                        ReceiptThread::Thread(thread_id.to_owned()),
                    )
                    .into_event(),
            ),
        )
        .await;

    // Then, subscribe the event cache.
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let (thread, _drop_handles) = event_cache.thread(room_id, thread_id).await.unwrap();
    let (_, mut thread_updates) = thread.subscribe().await.unwrap();
    assert!(thread_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk([
                f.text_msg("ev1")
                    .in_thread(thread_id, thread_id)
                    .event_id(event_id!("$1"))
                    .into_raw(),
                f.text_msg("ev2")
                    .in_thread(thread_id, thread_id)
                    .event_id(event_id!("$2"))
                    .into_raw(),
                f.text_msg("ev3")
                    .in_thread(thread_id, thread_id)
                    .event_id(event_id!("$3"))
                    .into_raw(),
            ]),
        )
        .await;

    assert_let_timeout!(Ok(_) = thread_updates.recv());

    // Only ev3 (after the receipt) is unread.
    assert_eq!(thread.num_unread_messages().await.unwrap(), 1);
}
