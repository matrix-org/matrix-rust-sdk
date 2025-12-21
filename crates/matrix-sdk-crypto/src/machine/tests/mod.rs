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

use std::{collections::BTreeMap, iter, ops::Not, sync::Arc, time::Duration};

use assert_matches2::{assert_let, assert_matches};
use futures_util::{FutureExt, StreamExt, pin_mut};
use itertools::Itertools;
use matrix_sdk_common::{
    deserialized_responses::{
        AlgorithmInfo, ProcessedToDeviceEvent, UnableToDecryptInfo, UnableToDecryptReason,
        UnsignedDecryptionResult, UnsignedEventLocation, VerificationLevel, VerificationState,
        WithheldCode,
    },
    executor::spawn,
};
use matrix_sdk_test::{async_test, message_like_event_content, ruma_response_from_json, test_json};
#[cfg(feature = "experimental-encrypted-state-events")]
use ruma::events::{
    StateEvent,
    room::topic::{OriginalRoomTopicEvent, RoomTopicEventContent},
};
use ruma::{
    DeviceId, DeviceKeyAlgorithm, DeviceKeyId, MilliSecondsSinceUnixEpoch, OneTimeKeyAlgorithm,
    RoomId, TransactionId, UserId,
    api::client::{
        keys::{get_keys, upload_keys},
        sync::sync_events::DeviceLists,
    },
    device_id,
    events::{
        AnyMessageLikeEvent, AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnyTimelineEvent,
        AnyToDeviceEvent, MessageLikeEvent, OriginalMessageLikeEvent, ToDeviceEventType,
        room::message::{
            AddMentions, MessageType, Relation, ReplyWithinThread, RoomMessageEventContent,
        },
    },
    room_id,
    serde::Raw,
    uint, user_id,
};
use serde_json::json;
use vodozemac::{
    Ed25519PublicKey,
    megolm::{GroupSession, SessionConfig},
};

use super::CrossSigningBootstrapRequests;
use crate::{
    Account, DecryptionSettings, DeviceData, EncryptionSettings, LocalTrust, MegolmError, OlmError,
    RoomEventDecryptionResult, TrustRequirement,
    error::{EventError, OlmResult},
    machine::{
        EncryptionSyncChanges, OlmMachine,
        test_helpers::{
            get_machine_after_query_test_helper, get_machine_pair_with_session,
            get_machine_pair_with_session_using_store,
            get_machine_pair_with_setup_sessions_test_helper, get_prepared_machine_test_helper,
        },
    },
    olm::{BackedUpRoomKey, ExportedRoomKey, SenderData, VerifyJson},
    session_manager::CollectStrategy,
    store::{
        CryptoStore, MemoryStore,
        types::{BackupDecryptionKey, Changes, DeviceChanges, PendingChanges, RoomKeyInfo},
    },
    types::{
        DeviceKeys, SignedKey, SigningKeys,
        events::{
            ToDeviceEvent,
            room::encrypted::{EncryptedToDeviceEvent, ToDeviceEncryptedEventContent},
            room_key_withheld::{MegolmV1AesSha2WithheldContent, RoomKeyWithheldContent},
        },
        requests::{AnyOutgoingRequest, ToDeviceRequest},
    },
    utilities::json_convert,
    verification::tests::bob_id,
};

mod decryption_verification_state;
mod interactive_verification;
mod megolm_sender_data;
mod olm_encryption;
mod room_settings;
mod send_encrypted_to_device;

fn alice_id() -> &'static UserId {
    user_id!("@alice:example.org")
}

fn alice_device_id() -> &'static DeviceId {
    device_id!("JLAFKJWSCS")
}

fn bob_device_id() -> &'static DeviceId {
    device_id!("NTHHPZDPRN")
}

fn user_id() -> &'static UserId {
    user_id!("@bob:example.com")
}

fn keys_upload_response() -> upload_keys::v3::Response {
    let json = &test_json::KEYS_UPLOAD;
    ruma_response_from_json(json)
}

fn keys_query_response() -> get_keys::v3::Response {
    let json = &test_json::KEYS_QUERY;
    ruma_response_from_json(json)
}

pub fn to_device_requests_to_content(
    requests: Vec<Arc<ToDeviceRequest>>,
) -> ToDeviceEncryptedEventContent {
    let to_device_request = &requests[0];
    assert_eq!(to_device_request.event_type, ToDeviceEventType::RoomEncrypted);

    to_device_request
        .messages
        .values()
        .next()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .deserialize_as_unchecked()
        .unwrap()
}

#[async_test]
async fn test_create_olm_machine() {
    let test_start_ts = MilliSecondsSinceUnixEpoch::now();
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;

    let device_creation_time = machine.device_creation_time();
    assert!(device_creation_time <= MilliSecondsSinceUnixEpoch::now());
    assert!(device_creation_time >= test_start_ts);

    let cache = machine.store().cache().await.unwrap();
    let account = cache.account().await.unwrap();
    assert!(!account.shared());

    let own_device = machine
        .get_device(machine.user_id(), machine.device_id(), None)
        .await
        .unwrap()
        .expect("We should always have our own device in the store");

    assert!(own_device.is_locally_trusted(), "Our own device should always be locally trusted");
}

#[async_test]
async fn test_generate_one_time_keys() {
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;

    machine
        .store()
        .with_transaction(|mut tr| async {
            let account = tr.account().await.unwrap();
            assert!(account.generate_one_time_keys_if_needed().is_some());
            Ok((tr, ()))
        })
        .await
        .unwrap();

    let mut response = keys_upload_response();

    machine.receive_keys_upload_response(&response).await.unwrap();

    machine
        .store()
        .with_transaction(|mut tr| async {
            let account = tr.account().await.unwrap();
            assert!(account.generate_one_time_keys_if_needed().is_some());
            Ok((tr, ()))
        })
        .await
        .unwrap();

    response.one_time_key_counts.insert(OneTimeKeyAlgorithm::SignedCurve25519, uint!(50));

    machine.receive_keys_upload_response(&response).await.unwrap();

    machine
        .store()
        .with_transaction(|mut tr| async {
            let account = tr.account().await.unwrap();
            assert!(account.generate_one_time_keys_if_needed().is_none());

            Ok((tr, ()))
        })
        .await
        .unwrap();
}

#[async_test]
async fn test_device_key_signing() {
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;

    let (device_keys, identity_keys) = {
        let cache = machine.store().cache().await.unwrap();
        let account = cache.account().await.unwrap();
        let device_keys = account.device_keys();
        let identity_keys = account.identity_keys();
        (device_keys, identity_keys)
    };

    let ed25519_key = identity_keys.ed25519;

    let ret = ed25519_key.verify_json(
        machine.user_id(),
        &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
        &device_keys,
    );
    ret.unwrap();
}

#[async_test]
async fn test_session_invalidation() {
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;
    let room_id = room_id!("!test:example.org");

    machine.create_outbound_group_session_with_defaults_test_helper(room_id).await.unwrap();
    assert!(machine.inner.group_session_manager.get_outbound_group_session(room_id).is_some());

    machine.discard_room_key(room_id).await.unwrap();

    assert!(
        machine
            .inner
            .group_session_manager
            .get_outbound_group_session(room_id)
            .unwrap()
            .invalidated()
    );
}

#[test]
fn test_invalid_signature() {
    let account = Account::with_device_id(user_id(), alice_device_id());

    let device_keys = account.device_keys();

    let key = Ed25519PublicKey::from_slice(&[0u8; 32]).unwrap();

    let ret = key.verify_json(
        account.user_id(),
        &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, account.device_id()),
        &device_keys,
    );
    ret.unwrap_err();
}

#[test]
fn test_one_time_key_signing() {
    let mut account = Account::with_device_id(user_id(), alice_device_id());
    account.update_uploaded_key_count(49);
    account.generate_one_time_keys_if_needed();

    let mut one_time_keys = account.signed_one_time_keys();
    let ed25519_key = account.identity_keys().ed25519;

    let one_time_key: SignedKey = one_time_keys
        .values_mut()
        .next()
        .expect("One time keys should be generated")
        .deserialize_as_unchecked()
        .unwrap();

    ed25519_key
        .verify_json(
            account.user_id(),
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, account.device_id()),
            &one_time_key,
        )
        .expect("One-time key has been signed successfully");
}

#[async_test]
async fn test_keys_for_upload() {
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let key_counts = BTreeMap::from([(OneTimeKeyAlgorithm::SignedCurve25519, 49u8.into())]);

    machine
        .receive_sync_changes(
            EncryptionSyncChanges {
                to_device_events: Vec::new(),
                changed_devices: &Default::default(),
                one_time_keys_counts: &key_counts,
                unused_fallback_keys: None,
                next_batch_token: None,
            },
            &decryption_settings,
        )
        .await
        .expect("We should be able to update our one-time key counts");

    let (ed25519_key, mut request) = {
        let cache = machine.store().cache().await.unwrap();
        let account = cache.account().await.unwrap();
        let ed25519_key = account.identity_keys().ed25519;

        let request =
            machine.keys_for_upload(&account).await.expect("Can't prepare initial key upload");
        (ed25519_key, request)
    };

    let one_time_key: SignedKey = request
        .one_time_keys
        .values_mut()
        .next()
        .expect("One time keys should be generated")
        .deserialize_as_unchecked()
        .unwrap();

    let ret = ed25519_key.verify_json(
        machine.user_id(),
        &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
        &one_time_key,
    );
    ret.unwrap();

    let device_keys: DeviceKeys = request.device_keys.unwrap().deserialize_as().unwrap();

    let ret = ed25519_key.verify_json(
        machine.user_id(),
        &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, machine.device_id()),
        &device_keys,
    );
    ret.unwrap();

    let response = {
        let cache = machine.store().cache().await.unwrap();
        let account = cache.account().await.unwrap();

        let mut response = keys_upload_response();
        response.one_time_key_counts.insert(
            OneTimeKeyAlgorithm::SignedCurve25519,
            account.max_one_time_keys().try_into().unwrap(),
        );

        response
    };

    machine.receive_keys_upload_response(&response).await.unwrap();

    {
        let cache = machine.store().cache().await.unwrap();
        let account = cache.account().await.unwrap();
        let ret = machine.keys_for_upload(&account).await;
        assert!(ret.is_none());
    }
}

#[async_test]
async fn test_keys_query() {
    let (machine, _) = get_prepared_machine_test_helper(user_id(), false).await;
    let response = keys_query_response();
    let alice_id = user_id!("@alice:example.org");
    let alice_device_id: &DeviceId = device_id!("JLAFKJWSCS");

    let alice_devices = machine.store().get_user_devices(alice_id).await.unwrap();
    assert!(alice_devices.devices().peekable().peek().is_none());

    let req_id = TransactionId::new();
    machine.receive_keys_query_response(&req_id, &response).await.unwrap();

    let device = machine.store().get_device(alice_id, alice_device_id).await.unwrap().unwrap();
    assert_eq!(device.user_id(), alice_id);
    assert_eq!(device.device_id(), alice_device_id);
}

#[async_test]
async fn test_query_keys_for_users() {
    let (machine, _) = get_prepared_machine_test_helper(user_id(), false).await;
    let alice_id = user_id!("@alice:example.org");
    let (_, request) = machine.query_keys_for_users(vec![alice_id]);
    assert!(request.device_keys.contains_key(alice_id));
}

#[async_test]
async fn test_missing_sessions_calculation() {
    let (machine, _) = get_machine_after_query_test_helper().await;

    let alice = alice_id();
    let alice_device = alice_device_id();

    let (_, missing_sessions) =
        machine.get_missing_sessions(iter::once(alice)).await.unwrap().unwrap();

    assert!(missing_sessions.one_time_keys.contains_key(alice));
    let user_sessions = missing_sessions.one_time_keys.get(alice).unwrap();
    assert!(user_sessions.contains_key(alice_device));
}

#[async_test]
async fn test_room_key_sharing() {
    let (alice, bob) = get_machine_pair_with_session(alice_id(), user_id(), false).await;
    let room_id = room_id!("!test:example.org");

    let (decrypted, room_key_updates) =
        send_room_key_to_device(&alice, &bob, room_id).await.unwrap();

    let event = decrypted[0].to_raw().deserialize().unwrap();

    if let AnyToDeviceEvent::RoomKey(event) = event {
        assert_eq!(&event.sender, alice.user_id());
        assert!(event.content.session_key.is_empty());
    } else {
        panic!("expected RoomKeyEvent found {event:?}");
    }

    let alice_session =
        alice.inner.group_session_manager.get_outbound_group_session(room_id).unwrap();

    let session = bob.store().get_inbound_group_session(room_id, alice_session.session_id()).await;

    assert!(session.unwrap().is_some());

    assert_eq!(room_key_updates.len(), 1);
    assert_eq!(room_key_updates[0].room_id, room_id);
    assert_eq!(room_key_updates[0].session_id, alice_session.session_id());
}

#[async_test]
async fn test_session_encryption_info_can_be_fetched() {
    // Given a megolm session has been established
    let (alice, bob) = get_machine_pair_with_session(alice_id(), user_id(), false).await;
    let room_id = room_id!("!test:example.org");

    send_room_key_to_device(&alice, &bob, room_id).await.unwrap();

    let alice_session =
        alice.inner.group_session_manager.get_outbound_group_session(room_id).unwrap();

    let session = bob
        .store()
        .get_inbound_group_session(room_id, alice_session.session_id())
        .await
        .unwrap()
        .unwrap();

    // When I request the encryption info about this session
    let encryption_info =
        bob.get_session_encryption_info(room_id, session.session_id(), alice_id()).await.unwrap();

    // Then the expected info is returned
    assert_eq!(encryption_info.sender, alice_id());
    assert_eq!(encryption_info.sender_device.as_deref(), Some(alice_device_id()));
    assert_matches!(
        &encryption_info.algorithm_info,
        AlgorithmInfo::MegolmV1AesSha2 { curve25519_key, .. }
    );
    assert_eq!(*curve25519_key, alice_session.sender_key().to_string());
    assert_eq!(
        encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::UnsignedDevice)
    );
}

#[async_test]
async fn test_to_device_messages_from_dehydrated_devices_are_ignored() {
    // Given alice's device is dehydrated
    let (alice, bob) = create_dehydrated_machine_and_pair().await;

    // When we send a to-device message from alice to bob
    // (Note: we send a room_key message, but it could be any to-device message.)
    let room_id = room_id!("!test:example.org");
    let (decrypted, room_key_updates) =
        send_room_key_to_device(&alice, &bob, room_id).await.unwrap();

    // Then the to-device message was discarded, because it was from a dehydrated
    // device
    assert!(decrypted.is_empty());

    // And the room key was not imported as a session
    let alice_session =
        alice.inner.group_session_manager.get_outbound_group_session(room_id).unwrap();
    let session = bob.store().get_inbound_group_session(room_id, alice_session.session_id()).await;
    assert!(session.unwrap().is_none());

    assert!(room_key_updates.is_empty());
}

/// "Send" a to-device message containing a room key from sender to receiver.
///
/// (Actually constructs the JSON of a to-device message from `sender` and feeds
/// it in to `receiver`'s `receive_sync_changes` method.
///
/// Returns the return value of `receive_sync_changes`, which is a tuple of
/// (decrypted to-device events, updated room keys).
async fn send_room_key_to_device(
    sender: &OlmMachine,
    receiver: &OlmMachine,
    room_id: &RoomId,
) -> OlmResult<(Vec<ProcessedToDeviceEvent>, Vec<RoomKeyInfo>)> {
    let to_device_requests = sender
        .share_room_key(room_id, iter::once(receiver.user_id()), EncryptionSettings::default())
        .await
        .unwrap();

    let event = ToDeviceEvent::new(
        sender.user_id().to_owned(),
        to_device_requests_to_content(to_device_requests),
    );
    let event = json_convert(&event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    receiver
        .receive_sync_changes(
            EncryptionSyncChanges {
                to_device_events: vec![event],
                changed_devices: &Default::default(),
                one_time_keys_counts: &Default::default(),
                unused_fallback_keys: None,
                next_batch_token: None,
            },
            &decryption_settings,
        )
        .await
}

/// Create an alice, bob pair where alice's device is dehydrated. Create a
/// session for messages from alice to bob, and ensure bob knows alice's device
/// is dehydrated.
async fn create_dehydrated_machine_and_pair() -> (OlmMachine, OlmMachine) {
    // Create a store holding info about an account that is linked to a dehydrated
    // device. This should never happen in real life, so we have to poke the
    // info into the store directly.
    let alice_store = MemoryStore::new();
    let alice_dehydrated_account = Account::new_dehydrated(alice_id());
    let mut alice_static_account = alice_dehydrated_account.static_data().clone();
    alice_static_account.dehydrated = true;
    let alice_device = DeviceData::from_account(&alice_dehydrated_account);
    let alice_dehydrated_device_id = alice_device.device_id().to_owned();
    alice_device.set_trust_state(LocalTrust::Verified);

    let changes = Changes {
        devices: DeviceChanges { new: vec![alice_device], ..Default::default() },
        ..Default::default()
    };
    alice_store.save_changes(changes).await.expect("Failed to same changes to the store");
    alice_store
        .save_pending_changes(PendingChanges { account: Some(alice_dehydrated_account) })
        .await
        .expect("Failed to save pending changes to the store");

    // Create the alice machine using the store we have made (and also create a
    // normal bob machine)
    get_machine_pair_with_session_using_store(
        alice_id(),
        user_id(),
        false,
        alice_store,
        &alice_dehydrated_device_id,
    )
    .await
}

#[async_test]
async fn test_request_missing_secrets() {
    let (alice, _) = get_machine_pair_with_session(alice_id(), bob_id(), false).await;

    let should_query_secrets = alice.query_missing_secrets_from_other_sessions().await.unwrap();

    assert!(should_query_secrets);

    let outgoing_to_device = alice
        .outgoing_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|outgoing| match outgoing.request.as_ref() {
            AnyOutgoingRequest::ToDeviceRequest(request) => {
                request.event_type.to_string() == "m.secret.request"
            }
            _ => false,
        })
        .collect_vec();

    assert_eq!(outgoing_to_device.len(), 4);

    // The second time, as there are already in-flight requests, it should have no
    // effect.
    let should_query_secrets_now = alice.query_missing_secrets_from_other_sessions().await.unwrap();
    assert!(!should_query_secrets_now);
}

#[async_test]
async fn test_request_missing_secrets_cross_signed() {
    let (alice, bob) = get_machine_pair_with_session(alice_id(), bob_id(), false).await;

    setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

    let should_query_secrets = alice.query_missing_secrets_from_other_sessions().await.unwrap();

    assert!(should_query_secrets);

    let outgoing_to_device = alice
        .outgoing_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|outgoing| match outgoing.request.as_ref() {
            AnyOutgoingRequest::ToDeviceRequest(request) => {
                request.event_type.to_string() == "m.secret.request"
            }
            _ => false,
        })
        .collect_vec();
    assert_eq!(outgoing_to_device.len(), 1);

    // The second time, as there are already in-flight requests, it should have no
    // effect.
    let should_query_secrets_now = alice.query_missing_secrets_from_other_sessions().await.unwrap();
    assert!(!should_query_secrets_now);
}

#[async_test]
async fn test_megolm_encryption() {
    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
    let room_id = room_id!("!test:example.org");

    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
        .await
        .unwrap();

    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        to_device_requests_to_content(to_device_requests),
    );

    let mut room_keys_received_stream = Box::pin(bob.store().room_keys_received_stream());

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let group_session = bob
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
        .unwrap()
        .inbound_group_session
        .unwrap();
    let sessions = std::slice::from_ref(&group_session);
    bob.store().save_inbound_group_sessions(sessions).await.unwrap();

    // when we decrypt the room key, the
    // inbound_group_session_streamroom_keys_received_stream should tell us
    // about it.
    let room_keys = room_keys_received_stream
        .next()
        .now_or_never()
        .flatten()
        .expect("We should have received an update of room key infos")
        .unwrap();
    assert_eq!(room_keys.len(), 1);
    assert_eq!(room_keys[0].session_id, group_session.session_id());

    let plaintext = "It is a secret to everybody";

    let content = RoomMessageEventContent::text_plain(plaintext);

    let result = alice
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
        .await
        .unwrap();

    let event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": result.content,
    });

    let event = json_convert(&event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let decryption_result =
        bob.try_decrypt_room_event(&event, room_id, &decryption_settings).await.unwrap();
    assert_let!(RoomEventDecryptionResult::Decrypted(decrypted_event) = decryption_result);
    let decrypted_event = decrypted_event.event.deserialize().unwrap();

    if let AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(
        MessageLikeEvent::Original(OriginalMessageLikeEvent { sender, content, .. }),
    )) = decrypted_event
    {
        assert_eq!(&sender, alice.user_id());
        if let MessageType::Text(c) = &content.msgtype {
            assert_eq!(&c.body, plaintext);
        } else {
            panic!("Decrypted event has a mismatched content");
        }
    } else {
        panic!("Decrypted room event has the wrong type");
    }

    // Just decrypting the event should *not* cause an update on the
    // inbound_group_session_stream.
    if let Some(igs) = room_keys_received_stream.next().now_or_never() {
        panic!("Session stream unexpectedly returned update: {igs:?}");
    }
}

/// Helper function to set up end-to-end Megolm encryption between two devices.
///
/// Creates two devices, Alice and Bob, and has Alice create an outgoing Megolm
/// session in the given room, whose decryption key is shared with Bob via a
/// to-device message.
///
/// # Arguments
///
/// * `room_id` - The RoomId for which to set up Megolm encryption.
///
/// # Returns
///
/// A tuple containing the alice and bob OlmMachine instances.
#[cfg(feature = "experimental-encrypted-state-events")]
async fn megolm_encryption_setup_helper(room_id: &RoomId) -> (OlmMachine, OlmMachine) {
    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;

    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
        .await
        .unwrap();

    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        to_device_requests_to_content(to_device_requests),
    );

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let group_session = bob
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
        .unwrap()
        .inbound_group_session
        .unwrap();
    let sessions = std::slice::from_ref(&group_session);
    bob.store().save_inbound_group_sessions(sessions).await.unwrap();

    (alice, bob)
}

/// Verifies that Megolm-encrypted state events can be encrypted and decrypted
/// correctly, and that the decrypted event matches the expected type and
/// content.
#[cfg(feature = "experimental-encrypted-state-events")]
#[async_test]
async fn test_megolm_state_encryption() {
    use ruma::events::{AnyStateEvent, EmptyStateKey};

    let room_id = room_id!("!test:example.org");
    let (alice, bob) = megolm_encryption_setup_helper(room_id).await;

    let plaintext = "It is a secret to everybody";
    let content = RoomTopicEventContent::new(plaintext.to_owned());
    let encrypted_content =
        alice.encrypt_state_event(room_id, content, EmptyStateKey).await.unwrap();

    let event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "state_key": "m.room.topic:",
        "content": encrypted_content,
    });

    let event = json_convert(&event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let decryption_result =
        bob.try_decrypt_room_event(&event, room_id, &decryption_settings).await.unwrap();

    assert_let!(RoomEventDecryptionResult::Decrypted(decrypted_event) = decryption_result);

    let decrypted_event = decrypted_event.event.deserialize().unwrap();

    if let AnyTimelineEvent::State(AnyStateEvent::RoomTopic(StateEvent::Original(
        OriginalRoomTopicEvent { sender, content, .. },
    ))) = decrypted_event
    {
        assert_eq!(&sender, alice.user_id());
        assert_eq!(&content.topic, plaintext);
    } else {
        panic!("Decrypted room event has the wrong type");
    }
}

/// Verifies that decryption fails with StateKeyVerificationFailed
/// when unpacking the state_key of the decrypted event yields an event type
/// that does not exist or does not match the type in the decrypted ciphertext.
#[cfg(feature = "experimental-encrypted-state-events")]
#[async_test]
async fn test_megolm_state_encryption_bad_type() {
    use ruma::events::EmptyStateKey;

    let room_id = room_id!("!test:example.org");
    let (alice, bob) = megolm_encryption_setup_helper(room_id).await;

    let plaintext = "It is a secret to everybody";
    let content = RoomTopicEventContent::new(plaintext.to_owned());
    let encrypted_content =
        alice.encrypt_state_event(room_id, content, EmptyStateKey).await.unwrap();

    let bad_type_event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "state_key": "m.room.malformed:",
        "content": encrypted_content,
    });

    let bad_type_event = json_convert(&bad_type_event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let bad_type_decryption_result =
        bob.try_decrypt_room_event(&bad_type_event, room_id, &decryption_settings).await.unwrap();

    assert_matches!(
        bad_type_decryption_result,
        RoomEventDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
            reason: UnableToDecryptReason::StateKeyVerificationFailed,
            ..
        })
    );
}

/// Verifies that decryption fails with StateKeyVerificationFailed
/// when unpacking the state_key of the decrypted event yields a state_key
/// that does not match the state_key in the decrypted ciphertext.
#[cfg(feature = "experimental-encrypted-state-events")]
#[async_test]
async fn test_megolm_state_encryption_bad_state_key() {
    use ruma::events::EmptyStateKey;

    let room_id = room_id!("!test:example.org");
    let (alice, bob) = megolm_encryption_setup_helper(room_id).await;

    let plaintext = "It is a secret to everybody";
    let content = RoomTopicEventContent::new(plaintext.to_owned());
    let encrypted_content =
        alice.encrypt_state_event(room_id, content, EmptyStateKey).await.unwrap();

    let bad_state_key_event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "state_key": "m.room.malformed:",
        "content": encrypted_content,
    });

    let bad_state_key_event = json_convert(&bad_state_key_event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let bad_state_key_decryption_result = bob
        .try_decrypt_room_event(&bad_state_key_event, room_id, &decryption_settings)
        .await
        .unwrap();

    assert_matches!(
        bad_state_key_decryption_result,
        RoomEventDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
            reason: UnableToDecryptReason::StateKeyVerificationFailed,
            ..
        })
    );
}

#[cfg(feature = "experimental-encrypted-state-events")]
#[async_test]
async fn test_megolm_state_encryption_outer_state_key_no_inner() {
    let room_id = room_id!("!test:example.org");
    let (alice, bob) = megolm_encryption_setup_helper(room_id).await;

    // Construct an inner message-like event and encrypt it.
    let plaintext = "It is a secret to everybody";
    let content = RoomMessageEventContent::text_plain(plaintext);
    let encrypted_content = alice
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content))
        .await
        .unwrap()
        .content;

    // Construct an outer event that has `state_key` defined.
    let event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "state_key": "m.room.message:",
        "content": encrypted_content,
    });

    let event = json_convert(&event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let decryption_result =
        bob.try_decrypt_room_event(&event, room_id, &decryption_settings).await.unwrap();

    assert_matches!(
        decryption_result,
        RoomEventDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
            reason: UnableToDecryptReason::StateKeyVerificationFailed,
            ..
        })
    );
}

#[cfg(feature = "experimental-encrypted-state-events")]
#[async_test]
async fn test_megolm_state_encryption_inner_state_key_no_outer() {
    use ruma::events::EmptyStateKey;

    let room_id = room_id!("!test:example.org");
    let (alice, bob) = megolm_encryption_setup_helper(room_id).await;

    // Construct an inner state event (with state key) and encrypt it.
    let plaintext = "It is a secret to everybody";
    let content = RoomTopicEventContent::new(plaintext.to_owned());
    let encrypted_content =
        alice.encrypt_state_event(room_id, content, EmptyStateKey).await.unwrap();

    // Construct an outer event without a state key.
    let event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": encrypted_content,
    });

    let event = json_convert(&event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let decryption_result =
        bob.try_decrypt_room_event(&event, room_id, &decryption_settings).await.unwrap();

    assert_matches!(
        decryption_result,
        RoomEventDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
            reason: UnableToDecryptReason::StateKeyVerificationFailed,
            ..
        })
    );
}

#[async_test]
async fn test_withheld_unverified() {
    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
    let room_id = room_id!("!test:example.org");

    let room_keys_withheld_received_stream = bob.store().room_keys_withheld_received_stream();
    pin_mut!(room_keys_withheld_received_stream);

    let encryption_settings = EncryptionSettings::default();
    let encryption_settings = EncryptionSettings {
        sharing_strategy: CollectStrategy::OnlyTrustedDevices,
        ..encryption_settings
    };

    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), encryption_settings)
        .await
        .expect("Share room key should be ok");

    // Here there will be only one request, and it's for a m.room_key.withheld

    // Transform that into an event to feed it back to bob machine
    let wh_content = to_device_requests[0]
        .messages
        .values()
        .next()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .deserialize_as_unchecked::<RoomKeyWithheldContent>()
        .expect("Deserialize should work");

    let event = ToDeviceEvent::new(alice.user_id().to_owned(), wh_content);

    let event = json_convert(&event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    bob.receive_sync_changes(
        EncryptionSyncChanges {
            to_device_events: vec![event],
            changed_devices: &Default::default(),
            one_time_keys_counts: &Default::default(),
            unused_fallback_keys: None,
            next_batch_token: None,
        },
        &decryption_settings,
    )
    .await
    .unwrap();

    // We should receive a notification on the room_keys_withheld_received_stream
    let withheld_received = room_keys_withheld_received_stream
        .next()
        .now_or_never()
        .flatten()
        .expect("We should have received a notification of room key being withheld");
    assert_eq!(withheld_received.len(), 1);

    assert_eq!(&withheld_received[0].room_id, room_id);
    assert_matches!(
        &withheld_received[0].withheld_event.content,
        RoomKeyWithheldContent::MegolmV1AesSha2(MegolmV1AesSha2WithheldContent::Unverified(
            unverified_withheld_content
        ))
    );
    assert_eq!(unverified_withheld_content.room_id, room_id);

    let plaintext = "You shouldn't be able to decrypt that message";

    let content = RoomMessageEventContent::text_plain(plaintext);

    let result = alice
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
        .await
        .unwrap();

    let room_event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": result.content,
    });
    let room_event = json_convert(&room_event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };
    let decrypt_result = bob.decrypt_room_event(&room_event, room_id, &decryption_settings).await;

    assert_matches!(&decrypt_result, Err(MegolmError::MissingRoomKey(Some(_))));

    let err = decrypt_result.err().unwrap();
    assert_matches!(err, MegolmError::MissingRoomKey(Some(WithheldCode::Unverified)));

    // Also check `try_decrypt_room_event`.
    let decrypt_result =
        bob.try_decrypt_room_event(&room_event, room_id, &decryption_settings).await.unwrap();
    assert_let!(RoomEventDecryptionResult::UnableToDecrypt(utd_info) = decrypt_result);
    assert!(utd_info.session_id.is_some());
    assert_eq!(
        utd_info.reason,
        UnableToDecryptReason::MissingMegolmSession {
            withheld_code: Some(WithheldCode::Unverified)
        }
    );
}

/// Test what happens when we feed an unencrypted event into the decryption
/// functions
#[async_test]
async fn test_decrypt_unencrypted_event() {
    let (bob, _) = get_prepared_machine_test_helper(user_id(), false).await;
    let room_id = room_id!("!test:example.org");

    let event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": user_id(),
        // it's actually the lack of an `algorithm` that upsets it, rather than the event type.
        "type": "m.room.encrypted",
        "content":  RoomMessageEventContent::text_plain("plain"),
    });

    let event = json_convert(&event).unwrap();

    // decrypt_room_event should return an error
    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };
    assert_matches!(
        bob.decrypt_room_event(&event, room_id, &decryption_settings).await,
        Err(MegolmError::JsonError(..))
    );

    // so should get_room_event_encryption_info
    assert_matches!(
        bob.get_room_event_encryption_info(&event, room_id).await,
        Err(MegolmError::JsonError(..))
    );
}

/// This only bootstrap cross-signing but it will not sign the current device !!
pub async fn setup_cross_signing_for_machine_test_helper(alice: &OlmMachine, bob: &OlmMachine) {
    let CrossSigningBootstrapRequests { upload_signing_keys_req: alice_upload_signing, .. } =
        alice.bootstrap_cross_signing(false).await.expect("Expect Alice x-signing key request");

    let CrossSigningBootstrapRequests { upload_signing_keys_req: bob_upload_signing, .. } =
        bob.bootstrap_cross_signing(false).await.expect("Expect Bob x-signing key request");

    let bob_device_keys = bob
        .get_device(bob.user_id(), bob.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .as_device_keys()
        .to_owned();

    let alice_device_keys = alice
        .get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .as_device_keys()
        .to_owned();

    // We only want to setup cross signing we don't actually sign the current
    // devices. so we ignore the new device signatures
    let json = json!({
        "device_keys": {
            bob.user_id() : { bob.device_id() : bob_device_keys},
            alice.user_id() : { alice.device_id():  alice_device_keys }
        },
        "failures": {},
        "master_keys": {
            bob.user_id() : bob_upload_signing.master_key.unwrap(),
            alice.user_id() : alice_upload_signing.master_key.unwrap()
        },
        "user_signing_keys": {
            bob.user_id() : bob_upload_signing.user_signing_key.unwrap(),
            alice.user_id() : alice_upload_signing.user_signing_key.unwrap()
        },
        "self_signing_keys": {
            bob.user_id() : bob_upload_signing.self_signing_key.unwrap(),
            alice.user_id() : alice_upload_signing.self_signing_key.unwrap()
        },
      }
    );

    let kq_response = ruma_response_from_json(&json);
    alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
}

async fn sign_alice_device_for_machine_test_helper(alice: &OlmMachine, bob: &OlmMachine) {
    let CrossSigningBootstrapRequests {
        upload_signing_keys_req: upload_signing,
        upload_signatures_req: upload_signature,
        ..
    } = alice.bootstrap_cross_signing(false).await.expect("Expect Alice x-signing key request");

    let mut device_keys = alice
        .get_device(alice.user_id(), alice.device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .as_device_keys()
        .to_owned();

    let raw_extracted =
        upload_signature.signed_keys.get(alice.user_id()).unwrap().iter().next().unwrap().1.get();

    let new_signature: DeviceKeys = serde_json::from_str(raw_extracted).unwrap();

    let self_sign_key_id = upload_signing
        .self_signing_key
        .as_ref()
        .unwrap()
        .get_first_key_and_id()
        .unwrap()
        .0
        .to_owned();

    device_keys.signatures.add_signature(
        alice.user_id().to_owned(),
        self_sign_key_id.to_owned(),
        new_signature.signatures.get_signature(alice.user_id(), &self_sign_key_id).unwrap(),
    );

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
#[cfg(feature = "automatic-room-key-forwarding")]
async fn test_query_ratcheted_key() {
    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
    let room_id = room_id!("!test:example.org");

    // Need a second bob session to check gossiping
    let bob_id = user_id();
    let bob_other_device = device_id!("OTHERBOB");
    let bob_other_machine = OlmMachine::new(bob_id, bob_other_device).await;
    let bob_other_device = DeviceData::from_machine_test_helper(&bob_other_machine).await.unwrap();
    bob.store().save_device_data(&[bob_other_device]).await.unwrap();
    bob.get_device(bob_id, device_id!("OTHERBOB"), None)
        .await
        .unwrap()
        .expect("should exist")
        .set_trust_state(LocalTrust::Verified);

    alice.create_outbound_group_session_with_defaults_test_helper(room_id).await.unwrap();

    let plaintext = "It is a secret to everybody";

    let content = RoomMessageEventContent::text_plain(plaintext);

    let result = alice
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
        .await
        .unwrap();

    let room_event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": result.content,
    });

    // should share at index 1
    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
        .await
        .unwrap();

    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        to_device_requests_to_content(to_device_requests),
    );

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let group_session = bob
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
        .unwrap()
        .inbound_group_session;
    bob.store().save_inbound_group_sessions(&[group_session.unwrap()]).await.unwrap();

    let room_event = json_convert(&room_event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };
    let decrypt_error =
        bob.decrypt_room_event(&room_event, room_id, &decryption_settings).await.unwrap_err();

    if let MegolmError::Decryption(vodo_error) = decrypt_error {
        if let vodozemac::megolm::DecryptionError::UnknownMessageIndex(_, _) = vodo_error {
            // check that key has been requested
            let outgoing_to_devices =
                bob.inner.key_request_machine.outgoing_to_device_requests().await.unwrap();
            assert_eq!(1, outgoing_to_devices.len());
        } else {
            panic!("Should be UnknownMessageIndex error ")
        }
    } else {
        panic!("Should have been unable to decrypt")
    }
}

#[async_test]
async fn test_room_key_over_megolm() {
    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
    let room_id = room_id!("!test:example.org");

    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
        .await
        .unwrap();

    let event = ToDeviceEvent {
        sender: alice.user_id().to_owned(),
        content: to_device_requests_to_content(to_device_requests),
        other: Default::default(),
    };
    let event = json_convert(&event).unwrap();
    let changed_devices = DeviceLists::new();
    let key_counts: BTreeMap<_, _> = Default::default();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let _ = bob
        .receive_sync_changes(
            EncryptionSyncChanges {
                to_device_events: vec![event],
                changed_devices: &changed_devices,
                one_time_keys_counts: &key_counts,
                unused_fallback_keys: None,
                next_batch_token: None,
            },
            &decryption_settings,
        )
        .await
        .unwrap();

    let group_session = GroupSession::new(SessionConfig::version_1());
    let session_key = group_session.session_key();
    let session_id = group_session.session_id();

    let content = message_like_event_content!({
        "algorithm": "m.megolm.v1.aes-sha2",
        "room_id": room_id,
        "session_id": session_id,
        "session_key": session_key.to_base64(),
    });

    let result = alice.encrypt_room_event_raw(room_id, "m.room_key", &content).await.unwrap();
    let event = json!({
        "sender": alice.user_id(),
        "content": result.content,
        "type": "m.room.encrypted",
    });

    let event: EncryptedToDeviceEvent = serde_json::from_value(event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let decrypt_result = bob
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
        .await;

    assert_matches!(decrypt_result, Err(OlmError::EventError(EventError::UnsupportedAlgorithm)));

    let event: Raw<AnyToDeviceEvent> = json_convert(&event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    bob.receive_sync_changes(
        EncryptionSyncChanges {
            to_device_events: vec![event],
            changed_devices: &changed_devices,
            one_time_keys_counts: &key_counts,
            unused_fallback_keys: None,
            next_batch_token: None,
        },
        &decryption_settings,
    )
    .await
    .unwrap();

    let session = bob.store().get_inbound_group_session(room_id, &session_id).await;

    assert!(session.unwrap().is_none());
}

#[async_test]
async fn test_room_key_with_fake_identity_keys() {
    let room_id = room_id!("!test:localhost");
    let (alice, _) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
    let device = DeviceData::from_machine_test_helper(&alice).await.unwrap();
    alice.store().save_device_data(&[device]).await.unwrap();

    let (outbound, mut inbound) = alice
        .store()
        .static_account()
        .create_group_session_pair(room_id, Default::default(), SenderData::unknown())
        .await
        .unwrap();

    let fake_key = Ed25519PublicKey::from_base64("ee3Ek+J2LkkPmjGPGLhMxiKnhiX//xcqaVL4RP6EypE")
        .unwrap()
        .into();
    let signing_keys = SigningKeys::from([(DeviceKeyAlgorithm::Ed25519, fake_key)]);
    inbound.creator_info.signing_keys = signing_keys.into();

    let content = message_like_event_content!({});
    let result = outbound.encrypt("m.dummy", &content).await;
    alice.store().save_inbound_group_sessions(&[inbound]).await.unwrap();

    let event = json!({
        "sender": alice.user_id(),
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "type": "m.room.encrypted",
        "content": result.content,
    });
    let event = json_convert(&event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };
    assert_matches!(
        alice.decrypt_room_event(&event, room_id, &decryption_settings).await,
        Err(MegolmError::MismatchedIdentityKeys { .. })
    );
}

#[async_test]
async fn test_importing_private_cross_signing_keys_verifies_the_public_identity() {
    async fn create_additional_machine(machine: &OlmMachine) -> OlmMachine {
        let second_machine = OlmMachine::new(machine.user_id(), "ADDITIONAL_MACHINE".into()).await;

        let identity = machine
            .get_identity(machine.user_id(), None)
            .await
            .unwrap()
            .expect("We should know about our own user identity if we bootstrapped it")
            .own()
            .unwrap();

        let mut changes = Changes::default();
        identity.mark_as_unverified();
        changes.identities.new.push(crate::UserIdentityData::Own(identity.inner));

        second_machine.store().save_changes(changes).await.unwrap();

        second_machine
    }

    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
    setup_cross_signing_for_machine_test_helper(&alice, &bob).await;

    let second_alice = create_additional_machine(&alice).await;

    let export = alice
        .export_cross_signing_keys()
        .await
        .unwrap()
        .expect("We should be able to export our cross-signing keys");

    let identity = second_alice
        .get_identity(second_alice.user_id(), None)
        .await
        .unwrap()
        .expect("We should know about our own user identity")
        .own()
        .unwrap();

    assert!(!identity.is_verified(), "Initially our identity should not be verified");

    second_alice
        .import_cross_signing_keys(export)
        .await
        .expect("We should be able to import our cross-signing keys");

    let identity = second_alice
        .get_identity(second_alice.user_id(), None)
        .await
        .unwrap()
        .expect("We should know about our own user identity")
        .own()
        .unwrap();

    assert!(
        identity.is_verified(),
        "Our identity should be verified after we imported the private cross-signing keys"
    );

    let second_bob = create_additional_machine(&bob).await;

    let export = second_alice
        .export_cross_signing_keys()
        .await
        .unwrap()
        .expect("The machine should now be able to export cross-signing keys as well");

    second_bob.import_cross_signing_keys(export).await.expect_err(
        "Importing cross-signing keys that don't match our public identity should fail",
    );

    let identity = second_bob
        .get_identity(second_bob.user_id(), None)
        .await
        .unwrap()
        .expect("We should know about our own user identity")
        .own()
        .unwrap();

    assert!(
        !identity.is_verified(),
        "Our identity should not be verified when there's a mismatch in the cross-signing keys"
    );
}

#[async_test]
async fn test_wait_on_key_query_doesnt_block_store() {
    // Waiting for a key query shouldn't delay other write attempts to the store.
    // This test will end immediately if it works, and times out after a few seconds
    // if it failed.

    let machine = OlmMachine::new(bob_id(), bob_device_id()).await;

    // Mark Alice as a tracked user, so it gets into the groups of users for which
    // we need to query keys.
    machine.update_tracked_users([alice_id()]).await.unwrap();

    // Start a background task that will wait for the key query to finish silently
    // in the background.
    let machine_cloned = machine.clone();
    let wait = spawn(async move {
        let machine = machine_cloned;
        let user_devices =
            machine.get_user_devices(alice_id(), Some(Duration::from_secs(10))).await.unwrap();
        assert!(user_devices.devices().next().is_some());
    });

    // Let the background task work first.
    tokio::task::yield_now().await;

    // Create a key upload request and process it back immediately.
    let requests = machine.bootstrap_cross_signing(false).await.unwrap();

    let req = requests.upload_keys_req.expect("upload keys request should be there");
    let response = keys_upload_response();
    let mark_request_as_sent = machine.mark_request_as_sent(&req.request_id, &response);
    tokio::time::timeout(Duration::from_secs(5), mark_request_as_sent)
        .await
        .expect("no timeout")
        .expect("the underlying request has been marked as sent");

    // Answer the key query, so the background task completes immediately?
    let response = keys_query_response();
    let key_queries = machine.inner.identity_manager.users_for_key_query().await.unwrap();

    for (id, _) in key_queries {
        machine.mark_request_as_sent(&id, &response).await.unwrap();
    }

    // The waiting should successfully complete.
    wait.await.unwrap();
}

#[async_test]
async fn test_fix_incorrect_usage_of_backup_key_causing_decryption_errors() {
    let store = MemoryStore::new();

    let backup_decryption_key = BackupDecryptionKey::new().unwrap();

    store
        .save_changes(Changes {
            backup_decryption_key: Some(backup_decryption_key.clone()),
            backup_version: Some("1".to_owned()),
            ..Default::default()
        })
        .await
        .unwrap();

    // Some valid key data
    let data = json!({
       "algorithm": "m.megolm.v1.aes-sha2",
       "room_id": "!room:id",
       "sender_key": "FOvlmz18LLI3k/llCpqRoKT90+gFF8YhuL+v1YBXHlw",
       "session_id": "/2K+V777vipCxPZ0gpY9qcpz1DYaXwuMRIu0UEP0Wa0",
       "session_key": "AQAAAAAclzWVMeWBKH+B/WMowa3rb4ma3jEl6n5W4GCs9ue65CruzD3ihX+85pZ9hsV9Bf6fvhjp76WNRajoJYX0UIt7aosjmu0i+H+07hEQ0zqTKpVoSH0ykJ6stAMhdr6Q4uW5crBmdTTBIsqmoWsNJZKKoE2+ldYrZ1lrFeaJbjBIY/9ivle++74qQsT2dIKWPanKc9Q2Gl8LjESLtFBD9Fmt",
       "sender_claimed_keys": {
           "ed25519": "F4P7f1Z0RjbiZMgHk1xBCG3KC4/Ng9PmxLJ4hQ13sHA"
       },
       "forwarding_curve25519_key_chain": ["DBPC2zr6c9qimo9YRFK3RVr0Two/I6ODb9mbsToZN3Q", "bBc/qzZFOOKshMMT+i4gjS/gWPDoKfGmETs9yfw9430"]
    });

    let backed_up_room_key: BackedUpRoomKey = serde_json::from_value(data).unwrap();

    // Create the machine using `with_store` and without a call to enable_backup_v1,
    // like regenerate_olm would do
    let alice = OlmMachine::with_store(user_id(), alice_device_id(), store, None).await.unwrap();

    let exported_key = ExportedRoomKey::from_backed_up_room_key(
        room_id!("!room:id").to_owned(),
        "/2K+V777vipCxPZ0gpY9qcpz1DYaXwuMRIu0UEP0Wa0".into(),
        backed_up_room_key,
    );

    alice.store().import_exported_room_keys(vec![exported_key], |_, _| {}).await.unwrap();

    let (_, request) = alice.backup_machine().backup().await.unwrap().unwrap();

    let key_backup_data = request.rooms[&room_id!("!room:id").to_owned()]
        .sessions
        .get("/2K+V777vipCxPZ0gpY9qcpz1DYaXwuMRIu0UEP0Wa0")
        .unwrap()
        .deserialize()
        .unwrap();

    let ephemeral = key_backup_data.session_data.ephemeral.encode();
    let ciphertext = key_backup_data.session_data.ciphertext.encode();
    let mac = key_backup_data.session_data.mac.encode();

    // Prior to the fix for GHSA-9ggc-845v-gcgv, this would produce a
    // `Mac(MacError)`
    backup_decryption_key
        .decrypt_v1(&ephemeral, &mac, &ciphertext)
        .expect("The backed up key should be decrypted successfully");
}

#[async_test]
async fn test_olm_machine_with_custom_account() {
    let store = MemoryStore::new();
    let account = vodozemac::olm::Account::new();
    let curve_key = account.identity_keys().curve25519;

    let alice =
        OlmMachine::with_store(user_id(), alice_device_id(), store, Some(account)).await.unwrap();

    assert_eq!(
        alice.identity_keys().curve25519,
        curve_key,
        "The Olm machine should have used the Account we provided"
    );
}

#[async_test]
async fn test_unsigned_decryption() {
    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
    let room_id = room_id!("!test:example.org");

    // Share the room key for the first message.
    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
        .await
        .unwrap();
    let first_room_key_event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        to_device_requests_to_content(to_device_requests),
    );

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    // Save the first room key.
    let group_session = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob
                .decrypt_to_device_event(
                    &mut tr,
                    &first_room_key_event,
                    &mut Changes::default(),
                    &decryption_settings,
                )
                .await?;
            Ok((tr, res))
        })
        .await
        .unwrap()
        .inbound_group_session;
    bob.store().save_inbound_group_sessions(&[group_session.unwrap()]).await.unwrap();

    // Encrypt first message.
    let first_message_text = "This is the original message";
    let first_message_content = RoomMessageEventContent::text_plain(first_message_text);
    let first_message_result =
        alice.encrypt_room_event(room_id, first_message_content).await.unwrap();

    let mut first_message_encrypted_event = json!({
        "event_id": "$message1",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": first_message_result.content,
    });
    let raw_encrypted_event = json_convert(&first_message_encrypted_event).unwrap();

    // Bob has the room key, so first message should be decrypted successfully.
    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };
    let raw_decrypted_event =
        bob.decrypt_room_event(&raw_encrypted_event, room_id, &decryption_settings).await.unwrap();

    let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
    assert_matches!(
        decrypted_event,
        AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
    );

    let first_message = first_message.as_original().unwrap();
    assert_eq!(first_message.content.body(), first_message_text);
    assert!(first_message.unsigned.relations.is_empty());

    assert!(raw_decrypted_event.unsigned_encryption_info.is_none());

    // Get a new room key, but don't give it to Bob yet.
    alice.discard_room_key(room_id).await.unwrap();
    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
        .await
        .unwrap();
    let second_room_key_event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        to_device_requests_to_content(to_device_requests),
    );

    // Encrypt a second message, an edit.
    let second_message_text = "This is the ~~original~~ edited message";
    let second_message_content =
        RoomMessageEventContent::text_plain(second_message_text).make_replacement(first_message);
    let second_message_result =
        alice.encrypt_room_event(room_id, second_message_content).await.unwrap();

    let second_message_encrypted_event = json!({
        "event_id": "$message2",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": second_message_result.content,
    });

    // Bundle the edit in the unsigned object of the first event.
    let relations = json!({
        "m.relations": {
            "m.replace": second_message_encrypted_event,
        },
    });
    first_message_encrypted_event.as_object_mut().unwrap().insert("unsigned".to_owned(), relations);
    let raw_encrypted_event = json_convert(&first_message_encrypted_event).unwrap();

    // Bob does not have the second room key, so second message should fail to
    // decrypt.
    let raw_decrypted_event =
        bob.decrypt_room_event(&raw_encrypted_event, room_id, &decryption_settings).await.unwrap();

    let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
    assert_matches!(
        decrypted_event,
        AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
    );

    let first_message = first_message.as_original().unwrap();
    assert_eq!(first_message.content.body(), first_message_text);
    // Deserialization of the edit failed, but it was here.
    assert!(first_message.unsigned.relations.replace.is_none());
    assert!(first_message.unsigned.relations.has_replacement());

    let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
    assert_eq!(unsigned_encryption_info.len(), 1);
    let replace_encryption_result =
        unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
    assert_matches!(
        replace_encryption_result,
        UnsignedDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
            session_id: Some(second_room_key_session_id),
            reason: UnableToDecryptReason::MissingMegolmSession { withheld_code: None },
        })
    );

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    // Give Bob the second room key.
    let group_session = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob
                .decrypt_to_device_event(
                    &mut tr,
                    &second_room_key_event,
                    &mut Changes::default(),
                    &decryption_settings,
                )
                .await?;
            Ok((tr, res))
        })
        .await
        .unwrap()
        .inbound_group_session
        .unwrap();
    assert_eq!(group_session.session_id(), second_room_key_session_id);
    bob.store().save_inbound_group_sessions(&[group_session]).await.unwrap();

    // Second message should decrypt now.
    let raw_decrypted_event =
        bob.decrypt_room_event(&raw_encrypted_event, room_id, &decryption_settings).await.unwrap();

    let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
    assert_matches!(
        decrypted_event,
        AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
    );

    let first_message = first_message.as_original().unwrap();
    assert_eq!(first_message.content.body(), first_message_text);
    let replace = first_message.unsigned.relations.replace.as_ref().unwrap();
    assert_matches!(&replace.content.relates_to, Some(Relation::Replacement(replace_content)));
    assert_eq!(replace_content.new_content.msgtype.body(), second_message_text);

    let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
    assert_eq!(unsigned_encryption_info.len(), 1);
    let replace_encryption_result =
        unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
    assert_matches!(replace_encryption_result, UnsignedDecryptionResult::Decrypted(_));

    // Get a new room key again, but don't give it to Bob yet.
    alice.discard_room_key(room_id).await.unwrap();
    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
        .await
        .unwrap();
    let third_room_key_event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        to_device_requests_to_content(to_device_requests),
    );

    // Encrypt a third message, a thread event.
    let third_message_text = "This a reply in a thread";
    let third_message_content = RoomMessageEventContent::text_plain(third_message_text)
        .make_for_thread(first_message, ReplyWithinThread::No, AddMentions::No);
    let third_message_result =
        alice.encrypt_room_event(room_id, third_message_content).await.unwrap();

    let third_message_encrypted_event = json!({
        "event_id": "$message3",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": third_message_result.content,
        "room_id": room_id,
    });

    // Bundle the edit in the unsigned object of the first event.
    let relations = json!({
        "m.relations": {
            "m.replace": second_message_encrypted_event,
            "m.thread": {
                "latest_event": third_message_encrypted_event,
                "count": 1,
                "current_user_participated": true,
            }
        },
    });
    first_message_encrypted_event.as_object_mut().unwrap().insert("unsigned".to_owned(), relations);
    let raw_encrypted_event = json_convert(&first_message_encrypted_event).unwrap();

    // Bob does not have the third room key, so third message should fail to
    // decrypt.
    let raw_decrypted_event =
        bob.decrypt_room_event(&raw_encrypted_event, room_id, &decryption_settings).await.unwrap();

    let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
    assert_matches!(
        decrypted_event,
        AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
    );

    let first_message = first_message.as_original().unwrap();
    assert_eq!(first_message.content.body(), first_message_text);
    assert!(first_message.unsigned.relations.replace.is_some());
    // Deserialization of the thread event succeeded, but it is still encrypted.
    let thread = first_message.unsigned.relations.thread.as_ref().unwrap();
    assert_matches!(
        thread.latest_event.deserialize(),
        Ok(AnySyncMessageLikeEvent::RoomEncrypted(_))
    );

    let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
    assert_eq!(unsigned_encryption_info.len(), 2);
    let replace_encryption_result =
        unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
    assert_matches!(replace_encryption_result, UnsignedDecryptionResult::Decrypted(_));
    let thread_encryption_result =
        unsigned_encryption_info.get(&UnsignedEventLocation::RelationsThreadLatestEvent).unwrap();
    assert_matches!(
        thread_encryption_result,
        UnsignedDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
            session_id: Some(third_room_key_session_id),
            reason: UnableToDecryptReason::MissingMegolmSession { withheld_code: None },
        })
    );

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    // Give Bob the third room key.
    let group_session = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob
                .decrypt_to_device_event(
                    &mut tr,
                    &third_room_key_event,
                    &mut Changes::default(),
                    &decryption_settings,
                )
                .await?;
            Ok((tr, res))
        })
        .await
        .unwrap()
        .inbound_group_session
        .unwrap();
    assert_eq!(group_session.session_id(), third_room_key_session_id);
    bob.store().save_inbound_group_sessions(&[group_session]).await.unwrap();

    // Third message should decrypt now.
    let raw_decrypted_event =
        bob.decrypt_room_event(&raw_encrypted_event, room_id, &decryption_settings).await.unwrap();

    let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
    assert_matches!(
        decrypted_event,
        AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
    );

    let first_message = first_message.as_original().unwrap();
    assert_eq!(first_message.content.body(), first_message_text);
    assert!(first_message.unsigned.relations.replace.is_some());
    let thread = &first_message.unsigned.relations.thread.as_ref().unwrap();
    assert_matches!(
        thread.latest_event.deserialize(),
        Ok(AnySyncMessageLikeEvent::RoomMessage(third_message))
    );
    let third_message = third_message.as_original().unwrap();
    assert_eq!(third_message.content.body(), third_message_text);

    let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
    assert_eq!(unsigned_encryption_info.len(), 2);
    let replace_encryption_result =
        unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
    assert_matches!(replace_encryption_result, UnsignedDecryptionResult::Decrypted(_));
    let thread_encryption_result =
        unsigned_encryption_info.get(&UnsignedEventLocation::RelationsThreadLatestEvent).unwrap();
    assert_matches!(thread_encryption_result, UnsignedDecryptionResult::Decrypted(_));
}

#[async_test]
async fn test_mark_all_tracked_users_as_dirty() {
    let store = MemoryStore::new();
    let account = vodozemac::olm::Account::new();

    // Put some tracked users
    let damir = user_id!("@damir:localhost");
    let ben = user_id!("@ben:localhost");
    let ivan = user_id!("@ivan:localhost");

    // Mark them as not dirty.
    store.save_tracked_users(&[(damir, false), (ben, false), (ivan, false)]).await.unwrap();

    // Let's imagine the migration has been done: this is useful so that tracked
    // users are not marked as dirty when creating the `OlmMachine`.
    store
        .set_custom_value(OlmMachine::key_for_has_migrated_verification_latch(), vec![0])
        .await
        .unwrap();

    let alice =
        OlmMachine::with_store(user_id(), alice_device_id(), store, Some(account)).await.unwrap();

    // All users are marked as not dirty.
    alice.store().load_tracked_users().await.unwrap().iter().for_each(|tracked_user| {
        assert!(tracked_user.dirty.not());
    });

    // Now, mark all tracked users as dirty.
    alice.mark_all_tracked_users_as_dirty().await.unwrap();

    // All users are now marked as dirty.
    alice.store().load_tracked_users().await.unwrap().iter().for_each(|tracked_user| {
        assert!(tracked_user.dirty);
    });
}

#[async_test]
async fn test_verified_latch_migration() {
    let store = MemoryStore::new();
    let account = vodozemac::olm::Account::new();

    // put some tracked users
    let bob_id = user_id!("@bob:localhost");
    let carol_id = user_id!("@carol:localhost");

    // Mark them as not dirty
    let to_track_not_dirty = vec![(bob_id, false), (carol_id, false)];
    store.save_tracked_users(&to_track_not_dirty).await.unwrap();

    let alice =
        OlmMachine::with_store(user_id(), alice_device_id(), store, Some(account)).await.unwrap();

    let alice_store = alice.store();

    // A migration should have occurred and all users should be marked as dirty
    alice_store.load_tracked_users().await.unwrap().iter().for_each(|tu| {
        assert!(tu.dirty);
    });

    // Ensure it does so only once
    alice_store.save_tracked_users(&to_track_not_dirty).await.unwrap();

    OlmMachine::migration_post_verified_latch_support(alice_store, alice.identity_manager())
        .await
        .unwrap();

    // Migration already done, so user should not be marked as dirty
    alice_store.load_tracked_users().await.unwrap().iter().for_each(|tu| {
        assert!(!tu.dirty);
    });
}
