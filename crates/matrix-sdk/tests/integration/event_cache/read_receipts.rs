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

use matrix_sdk::test_utils::mocks::MatrixMockServer;
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

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").event_id(event_id!("$1")))
                .add_timeline_event(f.text_msg("world").event_id(event_id!("$2"))),
        )
        .await;

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

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);
    let own_user_id = client.user_id().unwrap();

    let room = server
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

    let room = server
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

    // First sync: three messages from BOB, no receipt.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(f.text_msg("ev1").event_id(event_id!("$1")))
                    .add_timeline_event(f.text_msg("ev2").event_id(event_id!("$2")))
                    .add_timeline_event(f.text_msg("ev3").event_id(event_id!("$3"))),
            );
        })
        .await;

    let room = client.get_room(room_id).unwrap();
    assert_eq!(room.num_unread_messages(), 3);

    // Second sync: a read receipt for ev2 arrives, no new messages.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id).add_receipt(
                    f.read_receipts()
                        .add(
                            event_id!("$2"),
                            own_user_id,
                            ReceiptType::Read,
                            ReceiptThread::Unthreaded,
                        )
                        .into_event(),
                ),
            );
        })
        .await;

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

    // First sync: three messages from BOB plus a receipt for a future event.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
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
            );
        })
        .await;

    let room = client.get_room(room_id).unwrap();

    // All three events are unread because the receipt target is unknown.
    assert_eq!(room.num_unread_messages(), 3);
    // The receipt is stored as pending.
    let room_read_receipts = room.read_receipts();
    assert!(room_read_receipts.pending.iter().any(|id| id == event_id!("$future")));
    assert_eq!(room_read_receipts.pending.len(), 1);

    // Second sync: $future arrives along with another message.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(
                        f.text_msg("the future event").event_id(event_id!("$future")),
                    )
                    .add_timeline_event(f.text_msg("ev4").event_id(event_id!("$4"))),
            );
        })
        .await;

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

    // First sync: two messages.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(f.text_msg("ev1").event_id(event_id!("$1")))
                    .add_timeline_event(f.text_msg("ev2").event_id(event_id!("$2"))),
            );
        })
        .await;

    let room = client.get_room(room_id).unwrap();
    assert_eq!(room.num_unread_messages(), 2);

    // Second sync: one more message, still no receipt.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(f.text_msg("ev3").event_id(event_id!("$3"))),
            );
        })
        .await;

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

    let room = server
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

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk([
                f.text_msg("hello").event_id(event_id!("$1")).into_raw(),
                f.reaction(event_id!("$1"), "👍").event_id(event_id!("$2")).into(),
            ]),
        )
        .await;

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

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.redacted(*BOB, RedactedRoomMessageEventContent::new())),
        )
        .await;

    // Redacted events don't count as unread.
    assert_eq!(room.num_unread_messages(), 0);
}

/// Test that messages with mentions increment the number of mentions.
#[async_test]
async fn test_mentions_increments_unread_mentions() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*BOB);

    // For mentions to be properly counted, we need to have a member event for the
    // current user.
    let member_event = f
        .member(client.user_id().unwrap())
        .membership(MembershipState::Join)
        .event_id(event_id!("$member"));

    let room = server
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

    // The message counts as unread and also increments the mentions count.
    assert_eq!(room.num_unread_messages(), 1);
    assert_eq!(room.num_unread_mentions(), 1);
}
