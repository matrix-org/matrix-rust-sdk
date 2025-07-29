use std::sync::Arc;

use assert_matches2::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::pin_mut;
use matrix_sdk::{
    assert_next_with_timeout, encryption::EncryptionSettings, test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{JoinedRoomBuilder, StateTestEvent, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{RoomExt, TimelineItem};
use ruma::{device_id, event_id, room_id, user_id};
use serde_json::{Value, json};

// Helper function to test the redecryption of different event types.
async fn test_redecryption(
    event_type: &str,
    content: Value,
    assertion_function: impl FnOnce(&VectorDiff<Arc<TimelineItem>>),
) {
    let room_id = room_id!("!test:localhost");

    let alice_user_id = user_id!("@alice:localhost");
    let alice_device_id = device_id!("ALICEDEVICE");
    let bob_user_id = user_id!("@bob:localhost");
    let bob_device_id = device_id!("BOBDEVICE");

    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;

    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

    // First we create some clients.

    let alice = matrix_mock_server
        .client_builder_for_crypto_end_to_end(alice_user_id, alice_device_id)
        .on_builder(|builder| {
            builder
                .with_enable_share_history_on_invite(true)
                .with_encryption_settings(encryption_settings)
        })
        .build()
        .await;

    let bob = matrix_mock_server
        .client_builder_for_crypto_end_to_end(bob_user_id, bob_device_id)
        .on_builder(|builder| {
            builder
                .with_enable_share_history_on_invite(true)
                .with_encryption_settings(encryption_settings)
        })
        .build()
        .await;

    // Ensure that Alice and Bob are aware of their devices and identities.
    matrix_mock_server.exchange_e2ee_identities(&alice, &bob).await;

    let event_factory = EventFactory::new().room(room_id);
    let alice_member_event = event_factory.member(alice_user_id).into_raw();
    let bob_member_event = event_factory.member(bob_user_id).into_raw();

    // Let us now create a room for them.
    let room_builder = JoinedRoomBuilder::new(room_id)
        .add_state_event(StateTestEvent::Create)
        .add_state_event(StateTestEvent::Encryption);

    matrix_mock_server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_joined_room(room_builder.clone());
        })
        .await;

    matrix_mock_server
        .mock_sync()
        .ok_and_run(&bob, |builder| {
            builder.add_joined_room(room_builder);
        })
        .await;

    let room =
        alice.get_room(room_id).expect("Alice should have access to the room now that we synced");

    let bob_room = bob.get_room(room_id).expect("Bob should have access to the invited room");

    // Alice will send a single event to the room, but this will trigger a to-device
    // message containing the room key to be sent as well. We capture both the event
    // and the to-device message.

    let event_id = event_id!("$some_id");
    let (event_receiver, mock) =
        matrix_mock_server.mock_room_send().ok_with_capture(event_id, alice_user_id);
    let (_guard, room_key) = matrix_mock_server.mock_capture_put_to_device(alice_user_id).await;

    {
        let _guard = mock.mock_once().mount_as_scoped().await;

        matrix_mock_server
            .mock_get_members()
            .ok(vec![alice_member_event.clone(), bob_member_event.clone()])
            .mock_once()
            .mount()
            .await;

        room.send_raw(event_type, content)
            .await
            .expect("We should be able to send an initial message");
    };

    // Let's now see what Bob's timeline for the room does.

    let timeline =
        bob_room.timeline().await.expect("We should be able to build a timeline for the room");
    let (initial, stream) = timeline.subscribe().await;
    assert!(initial.is_empty(), "Initially we have an empty timeline");
    pin_mut!(stream);

    // Let us retrieve the captured event and to-device message.
    let event = event_receiver.await.expect("Alice should have sent the event by now");
    let room_key = room_key.await;

    // Let us forward the event to Bob.
    matrix_mock_server
        .mock_sync()
        .ok_and_run(&bob, |builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
        })
        .await;

    // This triggers a timeline update with a pushed item, the item is a UTD.
    let updates = assert_next_with_timeout!(stream);
    let utd_item = &updates[0];

    assert_matches!(utd_item, VectorDiff::PushBack { value });
    assert!(
        value.as_event().unwrap().content().is_unable_to_decrypt(),
        "Initially we should receive a UTD"
    );

    // Now we send the room key to Bob.
    matrix_mock_server
        .mock_sync()
        .ok_and_run(&bob, |builder| {
            builder.add_to_device_event(
                room_key.deserialize_as().expect("We should be able to deserialize the room key"),
            );
        })
        .await;

    // This should trigger a update.
    let updates = assert_next_with_timeout!(stream);
    let decrypted_item = &updates[0];

    // Let us run the assertion function with the decrypted item provided by the
    // caller.
    assertion_function(decrypted_item);
}

// Test ensuring that a late room key triggers a redecryption and the timeline
// item containing a UTD gets successfully replaced.
#[async_test]
async fn test_redecryption_after_late_to_device() {
    test_redecryption(
        "m.room.message",
        json!({"body": "It's a secret to everybody", "msgtype": "m.text"}),
        |decrypted_item| {
            assert_matches!(decrypted_item, VectorDiff::Set { index: _, value });

            let event = value.as_event().expect("The value should be an event");

            let content = event.content();
            assert!(!content.is_unable_to_decrypt(), "The UTD should now have been resolved");

            let content = content.as_message().expect("The event should contain a message");
            assert_eq!(content.body(), "It's a secret to everybody");
        },
    )
    .await;
}

// Test ensuring that a late room key triggers a redecryption of unsupported
// event type and the timeline item containing a UTD gets successfully removed.
#[async_test]
async fn test_redecryption_after_late_to_device_unsupported_event_type() {
    test_redecryption(
        "rust-sdk.custom.event",
        json!({"body": "It's a secret to everybody"}),
        |decrypted_item| {
            assert_matches!(decrypted_item, VectorDiff::Remove { index: _ });
        },
    )
    .await;
}
