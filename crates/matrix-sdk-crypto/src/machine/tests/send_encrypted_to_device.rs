// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use assert_matches2::{assert_let, assert_matches};
use insta::assert_json_snapshot;
use matrix_sdk_common::deserialized_responses::{
    AlgorithmInfo, ProcessedToDeviceEvent, ToDeviceUnableToDecryptReason, VerificationLevel,
    VerificationState,
};
use matrix_sdk_test::{async_test, ruma_response_from_json};
use ruma::{
    RoomId, TransactionId, events::AnyToDeviceEvent, room_id, serde::Raw,
    to_device::DeviceIdOrAllDevices,
};
use serde_json::{Value, json, value::to_raw_value};

use crate::{
    CrossSigningBootstrapRequests, DecryptionSettings, DeviceData, EncryptionSettings,
    EncryptionSyncChanges, LocalTrust, OlmError, OlmMachine, TrustRequirement,
    machine::{
        test_helpers::{
            build_encrypted_to_device_content_without_sender_data, build_session_for_pair,
            get_machine_pair, get_machine_pair_with_session, get_prepared_machine_test_helper,
            receive_encrypted_to_device_test_helper,
            send_and_receive_encrypted_to_device_test_helper,
        },
        tests::{self, decryption_verification_state::mark_alice_identity_as_verified_test_helper},
    },
    olm::SenderData,
    session_manager::CollectStrategy,
    types::{
        events::{
            EventType as _, ToDeviceCustomEvent, ToDeviceEvent,
            room::encrypted::ToDeviceEncryptedEventContent,
        },
        requests::ToDeviceRequest,
    },
    utilities::json_convert,
    verification::tests::bob_id,
};

/// Happy path test: encrypt a to-device message, and check it is successfully
/// decrypted by the recipient, and that all the metadata is set as expected.
#[async_test]
async fn test_send_encrypted_to_device() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { raw, encryption_info } = processed_event);

    let decrypted_event = raw.deserialize().unwrap();

    assert_eq!(decrypted_event.event_type().to_string(), custom_event_type.to_owned());

    let decrypted_value = to_raw_value(&raw).unwrap();
    let decrypted_value = serde_json::to_value(decrypted_value).unwrap();

    assert_eq!(
        decrypted_value.get("content").unwrap().get("device_id").unwrap().as_str().unwrap(),
        custom_content.get("device_id").unwrap().as_str().unwrap(),
    );

    assert_eq!(
        decrypted_value.get("content").unwrap().get("rooms").unwrap().as_array().unwrap(),
        custom_content.get("rooms").unwrap().as_array().unwrap(),
    );

    assert_eq!(encryption_info.sender, alice.user_id().to_owned());

    assert_matches!(&encryption_info.sender_device, Some(sender_device));
    assert_eq!(sender_device.to_owned(), alice.device_id().to_owned());

    assert_matches!(
        &encryption_info.algorithm_info,
        AlgorithmInfo::OlmV1Curve25519AesSha2 { curve25519_public_key_base64 }
    );
    let alice_device =
        alice.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();
    assert_eq!(
        curve25519_public_key_base64.to_owned(),
        alice_device.curve25519_key().unwrap().to_base64()
    );

    assert_matches!(
        &encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::UnsignedDevice)
    );
}

/// Test what happens when the sending device is deleted before the to-device
/// event arrives. (It should still be successfully decrypted.)
///
/// Regression test for https://github.com/matrix-org/matrix-rust-sdk/issues/5768.
#[async_test]
async fn test_encrypted_to_device_from_deleted_device() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    // Tell Bob that Alice's device has been deleted
    let mut keys_query_response = ruma::api::client::keys::get_keys::v3::Response::default();
    keys_query_response.device_keys.insert(alice.user_id().to_owned(), Default::default());
    bob.receive_keys_query_response(&TransactionId::new(), &keys_query_response).await.unwrap();

    let custom_event_type = "m.new_device";
    let custom_content = json!({"a": "b"});

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { raw, encryption_info } = processed_event);

    let decrypted_event = raw.deserialize().unwrap();
    assert_eq!(decrypted_event.event_type().to_string(), custom_event_type.to_owned());

    assert_eq!(encryption_info.sender, alice.user_id().to_owned());
    assert_matches!(&encryption_info.sender_device, Some(sender_device));
    assert_eq!(sender_device.to_owned(), alice.device_id().to_owned());
}

/// If the sender device is genuinely unknown (it is not in the store, nor does
/// the to-device message contain `sender_device_keys`), decryption will fail,
/// with `EventError::MissingSigningKey`.
#[async_test]
async fn test_receive_custom_encrypted_to_device_with_no_sender_device_keys_fails_if_device_unknown()
 {
    let (bob, otk) = get_prepared_machine_test_helper(bob_id(), false).await;

    let alice = OlmMachine::new(tests::alice_id(), tests::alice_device_id()).await;

    let bob_device = DeviceData::from_machine_test_helper(&bob).await.unwrap();
    alice.store().save_device_data(&[bob_device]).await.unwrap();

    let (alice, bob) = build_session_for_pair(alice, bob, otk).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    // We need to suppress the sender_data field to correctly emulate an unknown
    // device
    let bob_device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();
    let raw_encrypted = build_encrypted_to_device_content_without_sender_data(
        &alice,
        &bob_device.device_keys,
        custom_event_type,
        &custom_content,
    )
    .await;

    let processed_event = receive_encrypted_to_device_test_helper(
        alice.user_id(),
        &bob,
        &decryption_settings,
        Raw::new(&raw_encrypted).unwrap(),
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::UnableToDecrypt { utd_info, .. } = processed_event);
    assert_eq!(utd_info.reason, ToDeviceUnableToDecryptReason::DecryptionFailure);
}

#[async_test]
async fn test_excluding_insecure_means_custom_to_device_events_from_unverified_devices_are_utd() {
    // Given we are in "exclude insecure devices" mode
    let decryption_settings = DecryptionSettings {
        sender_device_trust_requirement: TrustRequirement::CrossSignedOrLegacy,
    };

    // Bob is the receiver
    let (bob, otk) = get_prepared_machine_test_helper(bob_id(), false).await;

    // Alice is the sender
    let alice = OlmMachine::new(tests::alice_id(), tests::alice_device_id()).await;

    let bob_device = DeviceData::from_machine_test_helper(&bob).await.unwrap();
    alice.store().save_device_data(&[bob_device]).await.unwrap();

    let (alice, bob) = build_session_for_pair(alice, bob, otk).await;

    // And the receiving device does not consider the sending device verified
    make_alice_unverified(&alice, &bob).await;

    assert!(
        !bob.get_device(alice.user_id(), alice.device_id(), None)
            .await
            .unwrap()
            .unwrap()
            .is_verified()
    );

    // When we send a custom event
    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    // Then it was not processed because the sending device was not verified
    assert_let!(ProcessedToDeviceEvent::UnableToDecrypt { utd_info, .. } = processed_event);

    // And the info provided in the UnableToDecrypt matches what we supplied
    assert_eq!(utd_info.reason, ToDeviceUnableToDecryptReason::UnverifiedSenderDevice);
}

#[async_test]
async fn test_excluding_insecure_does_not_prevent_key_events_being_processed() {
    // Given we are in "exclude insecure devices" mode
    let decryption_settings = DecryptionSettings {
        sender_device_trust_requirement: TrustRequirement::CrossSignedOrLegacy,
    };

    // Bob is the receiver
    let (bob, otk) = get_prepared_machine_test_helper(bob_id(), false).await;

    // Alice is the sender
    let alice = OlmMachine::new(tests::alice_id(), tests::alice_device_id()).await;

    let bob_device = DeviceData::from_machine_test_helper(&bob).await.unwrap();
    alice.store().save_device_data(&[bob_device]).await.unwrap();

    let (alice, bob) = build_session_for_pair(alice, bob, otk).await;

    // And the receiving device does not consider the sending device verified
    make_alice_unverified(&alice, &bob).await;

    assert!(
        !bob.get_device(alice.user_id(), alice.device_id(), None)
            .await
            .unwrap()
            .unwrap()
            .is_verified()
    );

    // When we send a room key event
    let key_event =
        create_and_share_session_without_sender_data(&alice, &bob, room_id!("!23:s.co")).await;

    let key_event_content = serde_json::to_value(&key_event.content).unwrap();

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        "m.room_key",
        &key_event_content,
        &decryption_settings,
    )
    .await;

    // Then it was processed because even though the sending device was not
    // verified, room key events are allowed through.
    assert_matches!(processed_event, ProcessedToDeviceEvent::Decrypted { .. });
}

#[async_test]
async fn test_send_olm_encryption_info_unverified_identity() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    // bootstrap cross-signing
    // tests::setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

    // sign alice device and let bob knows about it
    tests::sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = processed_event);
    assert_eq!(encryption_info.sender, alice.user_id().to_owned());
    assert_eq!(encryption_info.sender_device, Some(alice.device_id().to_owned()));
    assert_eq!(encryption_info.session_id(), None);

    assert_matches!(
        &encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
    );
}

#[async_test]
async fn test_send_olm_encryption_info_verified_identity() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    // bootstrap cross-signing
    tests::setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

    // sign alice device and let bob knows about it
    tests::sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    // Given alice is verified
    mark_alice_identity_as_verified_test_helper(&alice, &bob).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = processed_event);
    assert_eq!(encryption_info.sender, alice.user_id().to_owned());
    assert_eq!(encryption_info.sender_device, Some(alice.device_id().to_owned()));
    assert_eq!(encryption_info.session_id(), None);

    assert_matches!(&encryption_info.verification_state, VerificationState::Verified);
}

#[async_test]
async fn test_send_olm_encryption_info_verified_locally() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    bob.get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .set_local_trust(LocalTrust::Verified)
        .await
        .unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = processed_event);
    assert_eq!(encryption_info.sender, alice.user_id().to_owned());
    assert_eq!(encryption_info.sender_device, Some(alice.device_id().to_owned()));
    assert_eq!(encryption_info.session_id(), None);

    assert_matches!(&encryption_info.verification_state, VerificationState::Verified);
}

#[async_test]
async fn test_send_olm_encryption_info_verification_violation() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    // bootstrap cross-signing
    tests::setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

    // sign alice device and let bob knows about it
    tests::sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    // Given alice is verified
    mark_alice_identity_as_verified_test_helper(&alice, &bob).await;

    // Reset alice identity
    alice.bootstrap_cross_signing(true).await.unwrap();
    tests::setup_cross_signing_for_machine_test_helper(&alice, &bob).await;
    tests::sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let processed_event = send_and_receive_encrypted_to_device_test_helper(
        &alice,
        &bob,
        custom_event_type,
        &custom_content,
        &decryption_settings,
    )
    .await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = processed_event);
    assert_eq!(encryption_info.sender, alice.user_id().to_owned());
    assert_eq!(encryption_info.sender_device, Some(alice.device_id().to_owned()));
    assert_eq!(encryption_info.session_id(), None);

    assert_matches!(
        &encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::VerificationViolation)
    );
}

#[async_test]
async fn test_processed_to_device_variants() {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "rtc.call.encryption_keys";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "call_id": "",
            "keys": [
                {
                    "index": 0,
                    "key": "I7qrSrCR7Yo9B4iGVnR8IA"
                }
            ],
    });

    let device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();
    let raw_encrypted = device
        .encrypt_event_raw(custom_event_type, &custom_content, CollectStrategy::AllDevices)
        .await
        .expect("Should have encrypted the content");

    let request = ToDeviceRequest::new(
        bob.user_id(),
        DeviceIdOrAllDevices::DeviceId(tests::bob_device_id().to_owned()),
        "m.room.encrypted",
        raw_encrypted.cast(),
    );

    let encrypted_event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        tests::to_device_requests_to_content(vec![request.clone().into()]),
    );

    let custom_event = json!({
            "sender": "@alice:example.com",
            "type": "m.new_device",
            "content": {
                "device_id": "XYZABCDE",
                "rooms": ["!726s6s6q:example.com"]
            }
    });
    let clear_event = serde_json::from_value::<ToDeviceCustomEvent>(custom_event.clone()).unwrap();

    let encrypted_event = json_convert(&encrypted_event).unwrap();
    let clear_event = json_convert(&clear_event).unwrap();

    let malformed_event_no_type = json!({
        "sender": "@alice:example.com",
        "content": {
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
        }
    });
    let malformed_event: Raw<AnyToDeviceEvent> =
        serde_json::from_value(malformed_event_no_type).unwrap();

    let alice_curve = alice
        .get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .curve25519_key()
        .unwrap();
    let bob_curve = bob
        .get_device(bob.user_id(), bob.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .curve25519_key()
        .unwrap();

    // let's add a UTD event
    let utd_event = json!({
        "content": {
            "algorithm": "m.olm.v1.curve25519-aes-sha2",
            "ciphertext": {
                bob_curve.to_base64(): {
                    // this payload is just captured from a sync of some other element web with other users
                    "body": "Awogjvpx458CGhuo77HX/+tp1sxgRoCi7iAlzMvfrpbWoREQAiKACysX/p+ojr5QitCi9WRXNyamW2v2LTvoyWKtVaA2oHnYGR5s5RYhDfnIgh5MMSqqKlAbfqLvrbLovTYcKagCBbFnbA43f6zYM44buGgy8q70hMVH5WP6aK1E9Z3DVZ+8PnXQGpsrxvz2IsL6w0Nzl/qUyBEQFcgkjoDPawbsZRCllMgq2LQUyqlun6IgDTCozqsfxhDWpdfYGde4z16m34Ang7f5pH+BmPrFs6E1AO5+UbhhhS6NwWlfEtA6/9yfMxWLz1d2OrLh+QG7lYFAU9/CzIoPxaHKKr4JxgL9CjsmUPyDymWOWHP0jLi1NwpOv6hGpx0FgM7jJIMk6gWGgC5rEgEeTIwdrJh3F9OKTNSva5hvD9LomGk6tZgzQG6oap1e3wiOUyTt6S7BlyMppIu3RlIiNihZ9e17JEGiGDXOXzMJ6ISAgvGVgTP7+EvyEt2Wt4du7uBo/UvljRvVNu3I8tfItizPAOlvz460+aBDxk+sflJWt7OnhiyPnOCfopb+1RzqKVCnnPyVaP2f4BPf8qpn/f5YZk+5jJgBrGPiHzzmb3sQ5pC470s6+U3MpVFlFTG/xPBtMRMwPsbKoHfnRPqIqGu5dQ1Sw7T6taDXWjP450TvjxgHK5t2z1rLA2SXzAB1P8xbi6YXqQwxL6PvMNHn/TM0jiIQHYuqg5/RKLyhHybfP8JAjgNBw9z16wfKR/YoYFr7c+S4McQaMNa8v2SxGzhpCC3duAoK2qCWLEkYRO5cMCsGm/9bf8Q+//OykygBU/hdkT1eHUbexgALPLdfhzduutU7pbChg4T7SH7euh/3NLmS/SQvkmPfm3ckbh/Vlcj9CsXws/7MX/VJbhpbyzgBNtMnbG6tAeAofMa6Go/yMgiNBZIhLpAm31iUbUhaGm2IIlF/lsmSYEiBPoSVfFU44tetX2I/PBDGiBlzyU+yC2TOEBwMGxBE3WHbIe5/7sKW8xJF9t+HBfxIyW1QRtY3EKdEcuVOTyMxYzq3L5OKOOtPDHObYiiXg00mAgdQqgfkEAIfoRCOa2NYfTedwwo0S77eQ1sPvW5Hhf+Cm+bLibkWzaYHEZF+vyE9/Tn0tZGtH07RXfUyhp1vtTH49OBZHGkb/r+L8OjYJTST1dDCGqeGXO3uwYjoWHXtezLVHYgL+UOwcLJfMF5s9DQiqcfYXzp2kEWGsaetBFXcUWqq4RMHqlr6QfbxyuYLlQzc/AYA/MrT3J6nDpNLcvozH3RcIs8NcKcjdtjvgL0QGThy3RcecJQEDx3STrkkePL3dlyFCtVsmtQ0vjBBCxUgdySfxiobGGnpezZYi7q+Xz61GOZ9QqYmkcZOPzfNWeqtmzB7gqlH1gkFsK2yMAzKq2XCDFHvA7YAT3yMGiY06FcQ+2jyg7Bk2Q+AvjTG8hlPlmt6BZfW5cz1qx1apQn1qHXHrgfWcI52rApYQlNPOU1Uc8kZ8Ee6XUhhXBGY1rvZiKjKFG0PPuS8xo4/P7/u+gH5gItmEVDFL6giYPFsPpqAQkUN7hFoGiVZEjO4PwrLOmydsEcNOfACqrnUs08FQtvPg0sjHnxh6nh6FUQv93ukKl6+c9d+pCsN2xukrQ7Dog3nrjFZ6PrS5J0k9rDAOwTB55sfGXPZ2rATOK1WS4XcpsCtqwnYm4sGNc8ALMQkQ97zCnw8TcQwLvdUMlfbqQ5ykDQpQD68fITEDDHmBAeTCjpC713E6AhvOMwTJvjhd7hSkeOTRTmn9zXIVGNo1jSr8u0xO9uLGeWsV0+UlRLgp7/nsgfermjwNN8wj6MW3DHGS8UzzYfe9TGCeywqqIUTqgfXY48leGgB7twh4cl4jcOQniLATTvigIAQIvq/Uv8L45BGnkpKTdQ5F73gehXdVA",
                    "type": 1
                }
            },
            "org.matrix.msgid": "93ee0170aa7740d0ac9ee137e820302d",
            "sender_key": alice_curve.to_base64(),
        },
        "type": "m.room.encrypted",
        "sender": "@alice:example.org",
    });

    let utd_event: Raw<AnyToDeviceEvent> = serde_json::from_value(utd_event).unwrap();

    let sync_changes = EncryptionSyncChanges {
        to_device_events: vec![encrypted_event, clear_event, malformed_event, utd_event],
        changed_devices: &Default::default(),
        one_time_keys_counts: &Default::default(),
        unused_fallback_keys: None,
        next_batch_token: None,
    };

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let (processed, _) =
        bob.receive_sync_changes(sync_changes, &decryption_settings).await.unwrap();

    assert_eq!(4, processed.len());

    let processed_event = &processed[0];
    assert_matches!(processed_event, ProcessedToDeviceEvent::Decrypted { .. });

    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        assert_json_snapshot!(
            processed_event.to_raw().deserialize_as::<Value>().unwrap(),
             {
                 ".keys.ed25519" => "[sender_ed25519_key]",
                 r#"["sender_device_keys"].device_id"# => "[ABCDEFGH]",
                 r#"["sender_device_keys"].keys"# => "++REDACTED++",
                 r#"["sender_device_keys"].signatures"# => "++REDACTED++",
                 // Redacted because depending on feature flags
                 r#"["sender_device_keys"].algorithms"# => "++REDACTED++",
                 ".recipient_keys.ed25519" => "[recipient_sender_key]",
            }
        );
    });

    let processed_event = &processed[1];
    assert_matches!(processed_event, ProcessedToDeviceEvent::PlainText(_));

    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        assert_json_snapshot!(
            processed_event.to_raw().deserialize_as::<Value>().unwrap(),
        );
    });

    let processed_event = &processed[2];
    assert_matches!(processed_event, ProcessedToDeviceEvent::Invalid(_));

    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        assert_json_snapshot!(
            processed_event.to_raw().deserialize_as::<Value>().unwrap(),
        );
    });

    let processed_event = &processed[3];
    assert_matches!(processed_event, ProcessedToDeviceEvent::UnableToDecrypt { utd_info, .. });
    assert_eq!(utd_info.reason, ToDeviceUnableToDecryptReason::DecryptionFailure);

    insta::with_settings!({ prepend_module_to_snapshot => false }, {
        assert_json_snapshot!(
            processed_event.to_raw().deserialize_as::<Value>().unwrap(),
            {
                ".content.sender_key" => "[sender_ed25519_key]",
                ".content.ciphertext" => "[++REDACTED++]",
            }
        );
    });
}

#[async_test]
async fn test_send_encrypted_to_device_no_session() {
    let (alice, bob, _) = get_machine_pair(tests::alice_id(), tests::user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let encryption_result = alice
        .get_device(bob.user_id(), tests::bob_device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .encrypt_event_raw(custom_event_type, &custom_content, CollectStrategy::AllDevices)
        .await;

    assert_matches!(encryption_result, Err(OlmError::MissingSession));
}

/// Create a new [`OutboundGroupSession`], and build a to-device event to share
/// it with another [`OlmMachine`], *without* sending the MSC4147 sender data.
///
/// # Arguments
///
/// * `alice` - sending device.
/// * `bob` - receiving device.
/// * `room_id` - room to create a session for.
async fn create_and_share_session_without_sender_data(
    alice: &OlmMachine,
    bob: &OlmMachine,
    room_id: &RoomId,
) -> ToDeviceEvent<ToDeviceEncryptedEventContent> {
    let (outbound_session, _) = alice
        .inner
        .group_session_manager
        .get_or_create_outbound_session(
            room_id,
            EncryptionSettings::default(),
            SenderData::unknown(),
        )
        .await
        .unwrap();

    // In future, we might want to save the session to the store, to better match
    // the behaviour of the real implementation. See
    // `GroupSessionManager::share_room_key` for inspiration on how to do that.

    let bob_device = alice
        .get_device(bob.user_id(), bob.device_id(), None)
        .await
        .unwrap()
        .expect("Attempt to send message to unknown device");
    let room_key_content = outbound_session.as_content().await;

    let content = build_encrypted_to_device_content_without_sender_data(
        alice,
        &bob_device.device_keys,
        room_key_content.event_type(),
        &room_key_content,
    )
    .await;

    ToDeviceEvent::new(alice.user_id().to_owned(), content)
}

/// Simulate uploading keys for alice that mean bob thinks alice's device
/// exists, but is unverified.
async fn make_alice_unverified(alice: &OlmMachine, bob: &OlmMachine) {
    let CrossSigningBootstrapRequests { upload_signing_keys_req: upload_signing, .. } =
        alice.bootstrap_cross_signing(false).await.expect("Expect Alice x-signing key request");

    let device_keys = alice
        .get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .as_device_keys()
        .to_owned();

    let updated_keys_with_x_signing = json!({ device_keys.device_id.to_string(): device_keys });

    let json = json!({
        "device_keys": {
            alice.user_id() : updated_keys_with_x_signing
        },
        "failures": {},
        "master_keys": {
            alice.user_id() : upload_signing.master_key.unwrap(),
        },
        "user_signing_keys": {
            alice.user_id() : upload_signing.user_signing_key.unwrap(),
        },
        "self_signing_keys": {
            alice.user_id() : upload_signing.self_signing_key.unwrap(),
        },
      }
    );

    let kq_response = ruma_response_from_json(&json);
    alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
}

#[async_test]
/// Test that when we get an error when we try to encrypt to a device that
/// doesn't satisfy the share strategy.
async fn test_share_strategy_prevents_encryption() {
    use matrix_sdk_common::deserialized_responses::WithheldCode;
    use matrix_sdk_test::test_json::keys_query_sets::KeyDistributionTestData as DataSet;
    use ruma::TransactionId;

    use crate::CrossSigningKeyExport;

    // Create the local user (`@me`), and import the public identity keys
    let machine = OlmMachine::new(DataSet::me_id(), DataSet::me_device_id()).await;
    let keys_query = DataSet::me_keys_query_response();
    machine.mark_request_as_sent(&TransactionId::new(), &keys_query).await.unwrap();

    // Also import the private cross signing keys
    machine
        .import_cross_signing_keys(CrossSigningKeyExport {
            master_key: DataSet::MASTER_KEY_PRIVATE_EXPORT.to_owned().into(),
            self_signing_key: DataSet::SELF_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
            user_signing_key: DataSet::USER_SIGNING_KEY_PRIVATE_EXPORT.to_owned().into(),
        })
        .await
        .unwrap();

    let keys_query = DataSet::dan_keys_query_response();
    let txn_id = TransactionId::new();
    machine.mark_request_as_sent(&txn_id, &keys_query).await.unwrap();

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let encryption_result = machine
        .get_device(DataSet::dan_id(), DataSet::dan_unsigned_device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .encrypt_event_raw(custom_event_type, &custom_content, CollectStrategy::OnlyTrustedDevices)
        .await;

    assert_matches!(encryption_result, Err(OlmError::Withheld(WithheldCode::Unverified)));
}
