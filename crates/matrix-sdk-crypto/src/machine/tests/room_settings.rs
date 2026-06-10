use std::time::Duration;

use assert_matches2::assert_matches;
use matrix_sdk_test::async_test;
use ruma::room_id;

use crate::{
    OlmMachine, SetRoomSettingsError, machine::tests, store::types::RoomSettings,
    types::EventEncryptionAlgorithm,
};

#[async_test]
async fn test_room_settings_returns_none_for_unknown_room() {
    let machine = OlmMachine::new(tests::user_id(), tests::alice_device_id()).await;
    let settings = machine.room_settings(room_id!("!test2:localhost")).await.unwrap();
    assert!(settings.is_none());
}

#[async_test]
async fn test_stores_and_returns_room_settings() {
    let machine = OlmMachine::new(tests::user_id(), tests::alice_device_id()).await;
    let room_id = room_id!("!test:localhost");

    let settings = RoomSettings {
        algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
        #[cfg(feature = "experimental-encrypted-state-events")]
        encrypt_state_events: false,
        only_allow_trusted_devices: true,
        session_rotation_period: Some(Duration::from_secs(10)),
        session_rotation_period_messages: Some(1234),
    };

    machine.set_room_settings(room_id, &settings).await.unwrap();
    assert_eq!(machine.room_settings(room_id).await.unwrap(), Some(settings));
}

#[async_test]
async fn test_set_room_settings_rejects_invalid_algorithms() {
    let machine = OlmMachine::new(tests::user_id(), tests::alice_device_id()).await;
    let room_id = room_id!("!test:localhost");

    let err = machine
        .set_room_settings(
            room_id,
            &RoomSettings {
                algorithm: EventEncryptionAlgorithm::OlmV1Curve25519AesSha2,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_matches!(err, SetRoomSettingsError::InvalidSettings);
}

#[async_test]
async fn test_set_room_settings_rejects_changes() {
    let machine = OlmMachine::new(tests::user_id(), tests::alice_device_id()).await;
    let room_id = room_id!("!test:localhost");

    // Initial settings
    machine
        .set_room_settings(
            room_id,
            &RoomSettings { session_rotation_period_messages: Some(100), ..Default::default() },
        )
        .await
        .unwrap();

    // Now, modifying the settings should be rejected
    let err = machine
        .set_room_settings(
            room_id,
            &RoomSettings { session_rotation_period_messages: Some(1000), ..Default::default() },
        )
        .await
        .unwrap_err();

    assert_matches!(err, SetRoomSettingsError::EncryptionDowngrade);
}

#[async_test]
async fn test_set_room_settings_accepts_noop_changes() {
    let machine = OlmMachine::new(tests::user_id(), tests::alice_device_id()).await;
    let room_id = room_id!("!test:localhost");

    // Initial settings
    machine
        .set_room_settings(
            room_id,
            &RoomSettings { session_rotation_period_messages: Some(100), ..Default::default() },
        )
        .await
        .unwrap();

    // Same again; should be fine.
    machine
        .set_room_settings(
            room_id,
            &RoomSettings { session_rotation_period_messages: Some(100), ..Default::default() },
        )
        .await
        .unwrap();
}
