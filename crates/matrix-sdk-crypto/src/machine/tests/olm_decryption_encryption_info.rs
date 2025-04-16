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
use matrix_sdk_common::deserialized_responses::{
    ProcessedToDeviceEvent, VerificationLevel, VerificationState,
};
use matrix_sdk_test::async_test;
use ruma::TransactionId;
use serde_json::json;

use crate::{
    machine::{
        test_helpers::{
            bootstrap_requests_to_keys_query_response, build_session_for_pair,
            encrypt_to_device_helper, get_machine_pair_with_session,
            get_prepared_machine_test_helper,
        },
        tests,
        tests::{
            alice_device_id, alice_id,
            decryption_verification_state::mark_alice_identity_as_verified_test_helper,
        },
    },
    verification::tests::bob_id,
    DeviceData, OlmMachine,
};

#[async_test]
async fn test_to_device_verification_state_signed_device() {
    let (alice, bob) = get_machine_pair_with_session(alice_id(), tests::user_id(), false).await;

    let bootstrap_requests = alice.bootstrap_cross_signing(false).await.unwrap();
    let kq_response = bootstrap_requests_to_keys_query_response(bootstrap_requests);
    bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();

    let custom_event_type = "m.hello";

    let custom_content = json!({
            "what": "foo",
    });

    let received = encrypt_to_device_helper(&alice, &bob, custom_event_type, custom_content).await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = received);

    assert_matches!(
        encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
    );
}

#[async_test]
async fn test_to_device_verification_state_verified() {
    let (alice, bob) = get_machine_pair_with_session(alice_id(), tests::user_id(), false).await;

    let bootstrap_requests = alice.bootstrap_cross_signing(false).await.unwrap();
    let kq_response = bootstrap_requests_to_keys_query_response(bootstrap_requests);
    bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();

    let bootstrap_requests = bob.bootstrap_cross_signing(false).await.unwrap();
    let kq_response = bootstrap_requests_to_keys_query_response(bootstrap_requests);
    alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();

    mark_alice_identity_as_verified_test_helper(&alice, &bob).await;

    let custom_event_type = "m.hello";

    let custom_content = json!({
            "what": "foo",
    });

    let received = encrypt_to_device_helper(&alice, &bob, custom_event_type, custom_content).await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = received);

    assert_matches!(encryption_info.verification_state, VerificationState::Verified);
}

#[async_test]
async fn test_to_device_verification_state_verification_violation() {
    let (alice, bob) = get_machine_pair_with_session(alice_id(), tests::user_id(), false).await;

    let bootstrap_requests = alice.bootstrap_cross_signing(false).await.unwrap();
    let kq_response = bootstrap_requests_to_keys_query_response(bootstrap_requests);
    bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();

    // Simulate Alice's cross-signing key changing after having been verified by
    // setting the `previously_verified` flag
    let alice_identity =
        bob.store().get_identity(alice.user_id()).await.unwrap().unwrap().other().unwrap();
    alice_identity.mark_as_previously_verified().await.unwrap();

    let custom_event_type = "m.hello";

    let custom_content = json!({
            "what": "foo",
    });

    let received = encrypt_to_device_helper(&alice, &bob, custom_event_type, custom_content).await;

    assert_let!(ProcessedToDeviceEvent::Decrypted { encryption_info, .. } = received);

    assert_matches!(
        encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::VerificationViolation)
    );
}

#[async_test]
async fn test_to_device_verification_level_none() {
    // Arrange
    // Set up prepared machine where it is the first time ever bob will receive a
    // message from this alice device. So bob is not yet aware of alice device.
    let (bob, otk) = get_prepared_machine_test_helper(bob_id(), false).await;
    let alice_device = alice_device_id();
    let alice = OlmMachine::new(alice_id(), alice_device).await;
    // Bob doesn't know yet alice keys. Alice contact in an async way
    let bob_device = DeviceData::from_machine_test_helper(&bob).await.unwrap();
    alice.store().save_device_data(&[bob_device]).await.unwrap();
    // Let alice claim one otk and create the outbound session
    let (alice, bob) = build_session_for_pair(alice, bob, otk).await;

    // Act

    let custom_event_type = "m.hello";

    let custom_content = json!({
            "what": "foo",
    });

    let received = encrypt_to_device_helper(&alice, &bob, custom_event_type, custom_content).await;

    // The decryption will fail (MissingSigningKey) for custom to-device events if
    // the recipient does not know the sender.
    // The VerificationLevel::None can only happen for room_key not custom events.
    assert_matches!(received, ProcessedToDeviceEvent::UnableToDecrypt { .. });
}
