use matrix_sdk::{encryption::EncryptionSettings, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, StateTestEvent, async_test, event_factory::EventFactory};
use ruma::{
    device_id, event_id,
    events::{StateEventType, room::topic::RoomTopicEventContent},
    room_id, user_id,
};
use serde_json::json;

/// Verifies clients can send encrypted state events when the room is configured
/// for them.
#[async_test]
async fn test_room_encrypted_state_event_send() {
    let room_id = room_id!("!test:localhost");
    let alice_user_id = user_id!("@alice:localhost");
    let alice_device_id = device_id!("ALICEDEVICE");

    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;
    matrix_mock_server.mock_room_state_encryption().state_encrypted().mount().await;

    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

    let alice = matrix_mock_server
        .client_builder_for_crypto_end_to_end(alice_user_id, alice_device_id)
        .on_builder(|builder| {
            builder
                .with_enable_share_history_on_invite(true)
                .with_encryption_settings(encryption_settings)
        })
        .build()
        .await;

    matrix_mock_server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_state_event(StateTestEvent::Create)
                    .add_state_event(StateTestEvent::EncryptionWithEncryptedStateEvents),
            );
        })
        .await;

    let event_factory = EventFactory::new().room(room_id);
    let alice_member_event = event_factory.member(alice_user_id).into_raw();
    matrix_mock_server
        .mock_get_members()
        .ok(vec![alice_member_event.clone()])
        .mock_once()
        .mount()
        .await;

    matrix_mock_server
        .mock_room_send_state()
        .for_type(StateEventType::RoomEncrypted)
        .expect_access_token("TOKEN_0")
        .for_key("m.room.topic:".to_owned())
        .body_matches_partial_json(json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "device_id": "ALICEDEVICE",
        }))
        .ok(event_id!("$1:example.org"))
        .mock_once()
        .mount()
        .await;

    // Get room and send a topic state event.
    alice
        .get_room(room_id)
        .expect("Alice should have access to the room")
        .send_state_event(RoomTopicEventContent::new("Encrypted topic".to_owned()))
        .await
        .unwrap();
}
