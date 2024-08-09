/*
Copyright 2024 The Matrix.org Foundation C.I.C.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{fmt::Debug, iter, pin::Pin};

use assert_matches::assert_matches;
use futures_core::Stream;
use futures_util::{FutureExt, StreamExt};
use matrix_sdk_test::async_test;
use ruma::{room_id, user_id, RoomId, TransactionId, UserId};
use serde::Serialize;
use serde_json::json;

use crate::{
    machine::{
        test_helpers::get_machine_pair_with_setup_sessions_test_helper,
        tests::to_device_requests_to_content,
    },
    olm::{InboundGroupSession, SenderData},
    store::RoomKeyInfo,
    types::events::{room::encrypted::ToDeviceEncryptedEventContent, EventType, ToDeviceEvent},
    EncryptionSettings, EncryptionSyncChanges, OlmMachine, Session,
};

/// Test the behaviour when a megolm session is received from an unknown device,
/// and the device keys are not in the to-device message.
#[async_test]
async fn test_receive_megolm_session_from_unknown_device() {
    // Given Bob does not know about Alice's device
    let (alice, bob) = get_machine_pair().await;
    let mut bob_room_keys_received_stream = Box::pin(bob.store().room_keys_received_stream());

    // `get_machine_pair_with_setup_sessions_test_helper` tells Bob about Alice's
    // device keys, so to run this test, we need to make him forget them.
    forget_devices_for_user(&bob, alice.user_id()).await;

    // When Alice starts a megolm session and shares the key with Bob, *without*
    // sending the sender data.
    let room_id = room_id!("!test:example.org");
    let event = create_and_share_session_without_sender_data(&alice, &bob, room_id).await;

    // Bob receives the to-device message
    receive_to_device_event(&bob, &event).await;

    // Then Bob should know about the session, and it should have
    // `SenderData::UnknownDevice`.
    let room_key_info = get_room_key_received_update(&mut bob_room_keys_received_stream);
    let session = get_inbound_group_session_or_panic(&bob, &room_key_info).await;

    assert_matches!(
        session.sender_data,
        SenderData::UnknownDevice {legacy_session, owner_check_failed} => {
            assert!(legacy_session); // TODO: change when https://github.com/matrix-org/matrix-rust-sdk/pull/3785 lands
            assert!(!owner_check_failed);
        }
    );
}

/// Test the behaviour when a megolm session is received from a known, but
/// unsigned, device.
#[async_test]
async fn test_receive_megolm_session_from_known_device() {
    // Given Bob knows about Alice's device
    let (alice, bob) = get_machine_pair().await;
    let mut bob_room_keys_received_stream = Box::pin(bob.store().room_keys_received_stream());

    // When Alice shares a room key with Bob
    let room_id = room_id!("!test:example.org");
    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        to_device_requests_to_content(
            alice
                .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
                .await
                .unwrap(),
        ),
    );

    // Bob receives the to-device message
    receive_to_device_event(&bob, &event).await;

    // Then Bob should know about the session, and it should have
    // `SenderData::DeviceInfo`
    let room_key_info = get_room_key_received_update(&mut bob_room_keys_received_stream);
    let session = get_inbound_group_session_or_panic(&bob, &room_key_info).await;

    assert_matches!(
        session.sender_data,
        SenderData::DeviceInfo {legacy_session, ..} => {
            assert!(legacy_session); // TODO: change when https://github.com/matrix-org/matrix-rust-sdk/pull/3785 lands
        }
    );
}

/// Convenience wrapper for [`get_machine_pair_with_setup_sessions_test_helper`]
/// using standard user ids.
async fn get_machine_pair() -> (OlmMachine, OlmMachine) {
    get_machine_pair_with_setup_sessions_test_helper(
        user_id!("@alice:example.org"),
        user_id!("@bob:example.com"),
        false,
    )
    .await
}

/// Tell the given [`OlmMachine`] to forget about any keys it has seen for the
/// given user.
async fn forget_devices_for_user(machine: &OlmMachine, other_user: &UserId) {
    let mut keys_query_response = ruma::api::client::keys::get_keys::v3::Response::default();
    keys_query_response.device_keys.insert(other_user.to_owned(), Default::default());
    machine.receive_keys_query_response(&TransactionId::new(), &keys_query_response).await.unwrap();
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

    let olm_sessions = alice
        .store()
        .get_sessions(&bob.identity_keys().curve25519.to_base64())
        .await
        .unwrap()
        .unwrap();
    let mut olm_session: Session = olm_sessions.lock().await[0].clone();

    let room_key_content = outbound_session.as_content().await;
    let plaintext = serde_json::to_string(&json!({
        "sender": alice.user_id(),
        "sender_device": alice.device_id(),
        "keys": { "ed25519": alice.identity_keys().ed25519.to_base64() },
        // We deliberately do *not* include:
        // "org.matrix.msc4147.device_keys": alice_device_keys,
        "recipient": bob.user_id(),
        "recipient_keys": { "ed25519": bob.identity_keys().ed25519.to_base64() },
        "type": room_key_content.event_type(),
        "content": room_key_content,
    }))
    .unwrap();

    let ciphertext = olm_session.encrypt_helper(&plaintext).await;
    ToDeviceEvent::new(
        alice.user_id().to_owned(),
        olm_session.build_encrypted_event(ciphertext, None).await.unwrap(),
    )
}

/// Pipe a to-device event into an [`OlmMachine`].
async fn receive_to_device_event<C>(machine: &OlmMachine, event: &ToDeviceEvent<C>)
where
    C: EventType + Serialize + Debug,
{
    let event_json = serde_json::to_string(event).expect("Unable to serialize to-device message");

    machine
        .receive_sync_changes(EncryptionSyncChanges {
            to_device_events: vec![serde_json::from_str(&event_json).unwrap()],
            changed_devices: &Default::default(),
            one_time_keys_counts: &Default::default(),
            unused_fallback_keys: None,
            next_batch_token: None,
        })
        .await
        .expect("Error receiving to-device event");
}

/// Given the `room_keys_received_stream`, check that there is a pending update,
/// and pop it.
fn get_room_key_received_update(
    room_keys_received_stream: &mut Pin<Box<impl Stream<Item = Vec<RoomKeyInfo>>>>,
) -> RoomKeyInfo {
    room_keys_received_stream
        .next()
        .now_or_never()
        .flatten()
        .expect("We should have received an update of room key infos")
        .pop()
        .expect("Received an empty room key info update")
}

/// Load the inbound group session corresponding to an update from the
/// `room_keys_received_stream` from the given machine's store.
async fn get_inbound_group_session_or_panic(
    machine: &OlmMachine,
    room_key_info: &RoomKeyInfo,
) -> InboundGroupSession {
    machine
        .store()
        .get_inbound_group_session(&room_key_info.room_id, &room_key_info.session_id)
        .await
        .expect("Error loading inbound group session")
        .expect("Inbound group session not found")
}
