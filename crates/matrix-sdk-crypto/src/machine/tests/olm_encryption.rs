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

use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use assert_matches2::assert_let;
use matrix_sdk_common::deserialized_responses::{
    ProcessedToDeviceEvent, ToDeviceUnableToDecryptReason,
};
use matrix_sdk_test::async_test;
use ruma::{
    DeviceKeyAlgorithm, DeviceKeyId, SecondsSinceUnixEpoch,
    canonical_json::to_canonical_value,
    device_id,
    events::{AnyToDeviceEvent, dummy::ToDeviceDummyEventContent},
    user_id,
};
use serde_json::json;
use vodozemac::Ed25519SecretKey;

use crate::{
    DecryptionSettings, DeviceData, OlmMachine, TrustRequirement,
    machine::{
        test_helpers::{
            create_session, get_machine_pair, get_machine_pair_with_session,
            get_machine_pair_with_setup_sessions_test_helper,
        },
        tests::{self, megolm_sender_data::receive_to_device_event},
    },
    olm::utility::SignJson,
    store::types::Changes,
    types::{DeviceKeys, events::ToDeviceEvent},
};

#[async_test]
async fn test_session_creation() {
    let (alice_machine, bob_machine, mut one_time_keys) =
        get_machine_pair(tests::alice_id(), tests::user_id(), false).await;
    let (key_id, one_time_key) = one_time_keys.pop_first().unwrap();

    create_session(
        &alice_machine,
        bob_machine.user_id(),
        bob_machine.device_id(),
        key_id,
        one_time_key,
    )
    .await;

    let session = alice_machine
        .store()
        .get_sessions(&bob_machine.store().static_account().identity_keys().curve25519.to_base64())
        .await
        .unwrap()
        .unwrap();

    assert!(!session.lock().await.is_empty())
}

#[async_test]
async fn test_getting_most_recent_session() {
    let (alice_machine, bob_machine, mut one_time_keys) =
        get_machine_pair(tests::alice_id(), tests::user_id(), false).await;
    let (key_id, one_time_key) = one_time_keys.pop_first().unwrap();

    let device = alice_machine
        .get_device(bob_machine.user_id(), bob_machine.device_id(), None)
        .await
        .unwrap()
        .unwrap();

    assert!(device.get_most_recent_session().await.unwrap().is_none());

    create_session(
        &alice_machine,
        bob_machine.user_id(),
        bob_machine.device_id(),
        key_id,
        one_time_key.to_owned(),
    )
    .await;

    for _ in 0..10 {
        let (key_id, one_time_key) = one_time_keys.pop_first().unwrap();

        create_session(
            &alice_machine,
            bob_machine.user_id(),
            bob_machine.device_id(),
            key_id,
            one_time_key.to_owned(),
        )
        .await;
    }

    let mut changes = Changes::default();

    // Since the sessions are created quickly in succession and our timestamps have
    // a resolution in seconds, it's very likely that we're going to end up
    // with the same timestamps, so we manually masage them to be 10s apart.
    let session_id = {
        let sessions = alice_machine
            .store()
            .get_sessions(&bob_machine.identity_keys().curve25519.to_base64())
            .await
            .unwrap()
            .unwrap();

        let mut sessions = sessions.lock().await;

        let mut use_time = SystemTime::now();

        let mut session_id = None;

        // Iterate through the sessions skipping the first and last element so we know
        // that the correct session isn't the first nor the last one.
        let (_, sessions_slice) = sessions.as_mut_slice().split_last_mut().unwrap();

        for session in sessions_slice.iter_mut().skip(1) {
            session.creation_time = SecondsSinceUnixEpoch::from_system_time(use_time).unwrap();
            use_time += Duration::from_secs(10);

            session_id = Some(session.session_id().to_owned());
            changes.sessions.push(session.clone());
        }

        session_id.unwrap()
    };

    alice_machine.store().save_changes(changes).await.unwrap();

    let newest_session = device.get_most_recent_session().await.unwrap().unwrap();

    assert_eq!(
        newest_session.session_id(),
        session_id,
        "The session we found is the one that was most recently created"
    );
}

#[async_test]
async fn test_get_most_recent_session_of_device_with_no_curve_key() {
    let alice_machine =
        OlmMachine::new(user_id!("@alice:example.org"), device_id!("ALICE_DEVICE")).await;
    let bob_user_id = user_id!("@bob:example.com");
    let bob_device_id = device_id!("BOB_DEVICE");

    let bob_device_data = {
        // Create a device with no Curve25519 key. It has to have an Ed25519 key, and be
        // signed, for us to accept it
        let bob_signing_key = Ed25519SecretKey::new();

        // Generate the unsigned structure
        let mut bob_device_keys = DeviceKeys::new(
            bob_user_id.to_owned(),
            bob_device_id.to_owned(),
            vec![],
            BTreeMap::from([(
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, bob_device_id),
                bob_signing_key.public_key().into(),
            )]),
            Default::default(),
        );

        // Add the signature
        bob_device_keys.signatures.add_signature(
            bob_user_id.to_owned(),
            DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, bob_device_id),
            bob_signing_key
                .sign_json(to_canonical_value(&bob_device_keys).unwrap())
                .expect("Could not sign device data"),
        );

        DeviceData::try_from(&bob_device_keys).unwrap()
    };

    alice_machine.store().save_device_data(&[bob_device_data]).await.unwrap();

    // Now, fetch the device from the store, and the most recent session should be
    // None.
    let device = alice_machine.get_device(bob_user_id, bob_device_id, None).await.unwrap().unwrap();
    let newest_session = device.get_most_recent_session().await.unwrap();
    assert!(newest_session.is_none());
}

async fn olm_encryption_test_helper(use_fallback_key: bool) {
    let (alice, bob) =
        get_machine_pair_with_session(tests::alice_id(), tests::user_id(), use_fallback_key).await;

    let bob_device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

    let (_, content) = bob_device
        .encrypt("m.dummy", ToDeviceDummyEventContent::new())
        .await
        .expect("We should be able to encrypt a dummy event.");

    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        content
            .deserialize_as_unchecked()
            .expect("We should be able to deserialize the encrypted content"),
    );

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    // Decrypting the first time should succeed.
    let decrypted = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob
                .decrypt_to_device_event(
                    &mut tr,
                    &event,
                    &mut Changes::default(),
                    &decryption_settings,
                )
                .await?;
            Ok((tr, res))
        })
        .await
        .expect("We should be able to decrypt the event.")
        .result
        .raw_event
        .deserialize()
        .expect("We should be able to deserialize the decrypted event.");

    assert_let!(AnyToDeviceEvent::Dummy(decrypted) = decrypted);
    assert_eq!(&decrypted.sender, alice.user_id());

    // Replaying the event should now result in a decryption failure.
    bob.store()
        .with_transaction(|mut tr| async {
            let res = bob
                .decrypt_to_device_event(
                    &mut tr,
                    &event,
                    &mut Changes::default(),
                    &decryption_settings,
                )
                .await?;
            Ok((tr, res))
        })
        .await
        .expect_err(
            "Decrypting a replayed event should not succeed, even if it's a pre-key message",
        );
}

#[async_test]
async fn test_olm_encryption() {
    olm_encryption_test_helper(false).await;
}

#[async_test]
async fn test_olm_encryption_with_fallback_key() {
    olm_encryption_test_helper(true).await;
}

/// Encrypted to-device messages which hold a `sender_device_keys`, but that
/// data is unsigned, should not be decrypted.
#[async_test]
async fn test_decrypt_to_device_message_with_unsigned_sender_keys() {
    let (alice, bob) = get_machine_pair_with_setup_sessions_test_helper(
        tests::alice_id(),
        tests::user_id(),
        false,
    )
    .await;

    let mut alice_session = alice
        .get_device(bob.user_id(), bob.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .get_most_recent_session()
        .await
        .unwrap()
        .unwrap();

    let mut malformed_device_keys = alice_session.our_device_keys.clone();
    malformed_device_keys.signatures.clear();
    let plaintext = serde_json::to_string(&json!({
        "sender": alice.user_id(),
        "sender_device": alice.device_id(),
        "keys": { "ed25519": alice.identity_keys().ed25519.to_base64() },
        "recipient": bob.user_id(),
        "recipient_keys": { "ed25519": bob.identity_keys().ed25519.to_base64() },
        "type": "org.matrix.test",
        "content": {"a": "b"},
        "sender_device_keys": malformed_device_keys,
    }))
    .unwrap();

    let ciphertext = alice_session.encrypt_helper(&plaintext).await;
    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        alice_session.build_encrypted_event(ciphertext, None).await.unwrap(),
    );

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    // Bob receives the to-device message
    let (to_device_events, _) = receive_to_device_event(&bob, &event, &decryption_settings).await;

    let event = to_device_events.first().expect("Bob did not get a to-device event").clone();

    // The to-device event should remain encrypted.
    assert_let!(ProcessedToDeviceEvent::UnableToDecrypt { encrypted_event, utd_info } = event);
    assert_eq!(encrypted_event.get_field("type").unwrap(), Some("m.room.encrypted"));

    assert_eq!(utd_info.reason, ToDeviceUnableToDecryptReason::DecryptionFailure);
}
