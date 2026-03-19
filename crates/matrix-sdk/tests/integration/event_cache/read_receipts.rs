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
//! - then, run assertions on the room unread counts.
//!
//! This avoids potential race conditions where a sync could be done, but the
//! processing by the event cache isn't, at the time we check the unread counts.

use matrix_sdk::{
    assert_let_timeout, event_cache::RoomEventCacheUpdate, test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{
    event_id,
    events::{
        Mentions,
        receipt::{ReceiptThread, ReceiptType},
        room::{member::MembershipState, message::RedactedRoomMessageEventContent},
    },
    room_id,
};

/// Test that the unread count increases when new messages arrive and no read
/// receipt is known.
#[async_test]
async fn test_unread_count_new_message_no_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").event_id(event_id!("$1")))
                .add_timeline_event(f.text_msg("world").event_id(event_id!("$2"))),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // Both messages from BOB count as unread since there is no read receipt.
    assert_eq!(room.num_unread_messages(), 2);
}

/// Test that the unread count only includes messages after the last known read
/// receipt.
#[async_test]
async fn test_unread_count_new_message_with_known_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let own_user_id = client.user_id().unwrap();
    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("ev1").event_id(event_id!("$1")))
                .add_timeline_event(f.text_msg("ev2").event_id(event_id!("$2")))
                .add_timeline_event(f.text_msg("ev3").event_id(event_id!("$3")))
                // The current user has read up to ev1.
                .add_receipt(
                    f.read_receipts()
                        .add(
                            event_id!("$2"),
                            own_user_id,
                            ReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                ),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // Only ev3 (after the receipt) is unread.
    assert_eq!(room.num_unread_messages(), 1);
}

/// Test that a message sent by the current user creates an implicit read
/// receipt so that only messages after the user's own message count as unread.
#[async_test]
async fn test_unread_count_implicit_receipt_own_message() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);
    let own_user_id = client.user_id().unwrap();

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("ev1").event_id(event_id!("$1")))
                .add_timeline_event(f.text_msg("ev2").event_id(event_id!("$2")))
                // The current user sends ev3: implicit read receipt up to here.
                .add_timeline_event(f.text_msg("ev3").sender(own_user_id).event_id(event_id!("$3")))
                .add_timeline_event(f.text_msg("ev4").event_id(event_id!("$4")))
                .add_timeline_event(f.text_msg("ev5").event_id(event_id!("$5"))),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // ev4 and ev5 (after our own ev3) are unread; ev1/ev2/ev3 are read via
    // implicit receipt.
    assert_eq!(room.num_unread_messages(), 2);
    assert_eq!(room.read_receipts().latest_active.unwrap().event_id, event_id!("$3"));
}

/// Test that receiving only a new read receipt event (with no new messages)
/// decreases the unread count.
#[async_test]
async fn test_unread_count_receipt_only_no_new_message() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);
    let own_user_id = client.user_id().unwrap();

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    // First sync: three messages from BOB, no receipt.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("ev1").event_id(event_id!("$1")))
                .add_timeline_event(f.text_msg("ev2").event_id(event_id!("$2")))
                .add_timeline_event(f.text_msg("ev3").event_id(event_id!("$3"))),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    assert_eq!(room.num_unread_messages(), 3);

    // Second sync: a read receipt for ev2 arrives, no new messages.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(event_id!("$2"), own_user_id, ReceiptType::Read, ReceiptThread::Unthreaded)
                    .into_event(),
            ),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // Only ev3 (after the receipt) is unread now.
    assert_eq!(room.num_unread_messages(), 1);
}

/// Test that a read receipt for an unknown event is stored as pending, and
/// resolves once that event arrives in a subsequent sync.
#[async_test]
async fn test_unread_count_pending_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);
    let own_user_id = client.user_id().unwrap();

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    // First sync: three messages from BOB plus a receipt for a future event.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("ev1").event_id(event_id!("$1")))
                .add_timeline_event(f.text_msg("ev2").event_id(event_id!("$2")))
                .add_timeline_event(f.text_msg("ev3").event_id(event_id!("$3")))
                // Receipt refers to $future, which isn't known yet.
                .add_receipt(
                    f.read_receipts()
                        .add(
                            event_id!("$future"),
                            own_user_id,
                            ReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                ),
        )
        .await;

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents(..)) = room_cache_updates.recv()
    );
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::AddEphemeralEvents { .. }) = room_cache_updates.recv()
    );

    // All three events are unread because the receipt target is unknown.
    assert_eq!(room.num_unread_messages(), 3);
    // The receipt is stored as pending.
    let room_read_receipts = room.read_receipts();
    assert!(room_read_receipts.pending.iter().any(|id| id == event_id!("$future")));
    assert_eq!(room_read_receipts.pending.len(), 1);

    // Second sync: $future arrives along with another message.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("the future event").event_id(event_id!("$future")))
                .add_timeline_event(f.text_msg("ev4").event_id(event_id!("$4"))),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // The pending receipt resolves: only ev4 (after $future) is unread.
    assert_eq!(room.num_unread_messages(), 1);
    assert!(room.read_receipts().pending.is_empty());
}

/// Test that unread counts accumulate across multiple syncs when no read
/// receipt is updated.
#[async_test]
async fn test_unread_count_accumulates_across_syncs() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    // First sync: two messages.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("ev1").event_id(event_id!("$1")))
                .add_timeline_event(f.text_msg("ev2").event_id(event_id!("$2"))),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    assert_eq!(room.num_unread_messages(), 2);

    // Second sync: one more message, still no receipt.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("ev3").event_id(event_id!("$3"))),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // Three messages are now unread in total.
    assert_eq!(room.num_unread_messages(), 3);
}

/// Test that state events (e.g. a membership change) in the timeline do not
/// increment the unread count.
#[async_test]
async fn test_state_event_does_not_increment_unread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id);

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    server
        .sync_room(
            &client,
            // BOB joining the room is a state event, not a message.
            JoinedRoomBuilder::new(room_id).add_timeline_state_bulk([f
                .member(*BOB)
                .membership(MembershipState::Join)
                .event_id(event_id!("$1"))
                .into_raw_sync_state()]),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    assert_eq!(room.num_unread_messages(), 0);
}

/// Test that reactions don't count in the unread message count.
#[async_test]
async fn test_reaction_does_not_increment_unread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk([
                f.text_msg("hello").event_id(event_id!("$1")).into_raw(),
                f.reaction(event_id!("$1"), "👍").event_id(event_id!("$2")).into(),
            ]),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // Only the text message counts as unread, the reaction doesn't.
    assert_eq!(room.num_unread_messages(), 1);
}

/// Test that redactions don't count in the unread message count.
#[async_test]
async fn test_redaction_does_not_increment_unread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.redacted(*BOB, RedactedRoomMessageEventContent::new())),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // Redacted events don't count as unread.
    assert_eq!(room.num_unread_messages(), 0);
}

/// Test unread count behavior across a gappy sync followed by a normal sync.
///
/// `update_read_receipts()` runs before `shrink_to_last_chunk()` inside
/// `handle_sync()`, so the unread count is recomputed against the pre-gap
/// events and stays unchanged immediately after the gappy sync. The shrink
/// then clears those events from memory, so the *subsequent* normal sync only
/// sees the newly-arrived event when recomputing, yielding a count of 1.
#[async_test]
async fn test_gappy_sync_keeps_then_next_sync_resets_unread_count() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    // First sync: two messages from BOB, no read receipt → unread count becomes 2.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").event_id(event_id!("$1")))
                .add_timeline_event(f.text_msg("world").event_id(event_id!("$2"))),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    assert_eq!(room.num_unread_messages(), 2);

    // Gappy sync: limited timeline with a prev_batch token and no new events.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .set_timeline_limited()
                .set_timeline_prev_batch("prev-batch".to_owned()),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // The unread count is recomputed while "$1" and "$2" are still in the linked
    // chunk (shrinking happens after), so it remains 2.
    assert_eq!(room.num_unread_messages(), 2);

    // Normal (non-gappy) sync: one new message from BOB.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("again").event_id(event_id!("$3"))),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // The gappy sync cleared "$1" and "$2" from the linked chunk, so this sync
    // only sees "$3" when recomputing the unread count, yielding 1. But this is
    // incorrect, as the number should be *at least* 2, and the SDK should keep
    // on showing this number in this case.
    assert_eq!(room.num_unread_messages(), 2);
}

/// Test that messages with mentions increment the number of mentions.
#[async_test]
async fn test_mentions_increments_unread_mentions() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

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
                        .event_id(event_id!("$1"))
                        .mentions(Mentions::with_user_ids([client.user_id().unwrap().to_owned()])),
                )
                .add_state_event(member_event),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // The message counts as unread and also increments the mentions count.
    assert_eq!(room.num_unread_messages(), 1);
    assert_eq!(room.num_unread_mentions(), 1);
}

/// Test that the unread count computation doesn't skip a more-recent active
/// receipt, when iterating over events.
#[async_test]
async fn test_compute_unread_counts_considers_active_receipt() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let own_user_id = client.user_id().unwrap();

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);

    let room = server.sync_joined_room(&client, room_id).await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
    assert!(room_cache_updates.is_empty());

    // Starting with a room with 1 implicit receipt, then two messages from Bob, and
    // a receipt on Bob's first message $2,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("hello 1").sender(own_user_id).event_id(event_id!("$1")),
                )
                .add_timeline_event(f.text_msg("hello 2").event_id(event_id!("$2")))
                .add_timeline_event(f.text_msg("hello 3").event_id(event_id!("$3")))
                .add_receipt(
                    f.read_receipts()
                        .add(
                            event_id!("$2"),
                            own_user_id,
                            ReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                ),
        )
        .await;

    // (one vector diff update, one read receipt update)
    assert_let_timeout!(Ok(_) = room_cache_updates.recv());
    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // The message counts are properly updated (one new message unread after $2).
    assert_eq!(room.num_unread_messages(), 1);

    // Provided a sync with one new message from Bob in the same room,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello 4").event_id(event_id!("$4"))),
        )
        .await;

    assert_let_timeout!(Ok(_) = room_cache_updates.recv());

    // The message counts are properly updated (two messages after $2).
    assert_eq!(room.num_unread_messages(), 2);
}
