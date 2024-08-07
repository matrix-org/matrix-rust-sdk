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
    iter,
    sync::Arc,
    time::{Duration, SystemTime},
};

use assert_matches2::{assert_let, assert_matches};
use futures_util::{pin_mut, FutureExt, StreamExt};
use itertools::Itertools;
use matrix_sdk_common::deserialized_responses::{
    DeviceLinkProblem, ShieldState, UnableToDecryptInfo, UnsignedDecryptionResult,
    UnsignedEventLocation, VerificationLevel, VerificationState,
};
use matrix_sdk_test::{async_test, message_like_event_content, test_json};
use ruma::{
    api::{
        client::{
            keys::{
                get_keys::{self, v3::Response as KeyQueryResponse},
                upload_keys,
            },
            sync::sync_events::DeviceLists,
            to_device::send_event_to_device::v3::Response as ToDeviceResponse,
        },
        IncomingResponse,
    },
    device_id,
    events::{
        dummy::ToDeviceDummyEventContent,
        key::verification::VerificationMethod,
        room::message::{
            AddMentions, MessageType, Relation, ReplyWithinThread, RoomMessageEventContent,
        },
        AnyMessageLikeEvent, AnyMessageLikeEventContent, AnyTimelineEvent, AnyToDeviceEvent,
        MessageLikeEvent, OriginalMessageLikeEvent,
    },
    room_id,
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
    uint, user_id, DeviceId, DeviceKeyAlgorithm, DeviceKeyId, MilliSecondsSinceUnixEpoch, RoomId,
    SecondsSinceUnixEpoch, TransactionId, UserId,
};
use serde_json::{json, value::to_raw_value};
use vodozemac::{
    megolm::{GroupSession, SessionConfig},
    Curve25519PublicKey, Ed25519PublicKey,
};

use super::{testing::response_from_file, CrossSigningBootstrapRequests};
use crate::{
    error::{EventError, SetRoomSettingsError},
    machine::{
        test_helpers::{
            create_session, get_machine_after_query_test_helper, get_machine_pair,
            get_machine_pair_with_session, get_machine_pair_with_setup_sessions_test_helper,
            get_prepared_machine_test_helper,
        },
        EncryptionSyncChanges, OlmMachine,
    },
    olm::{
        BackedUpRoomKey, ExportedRoomKey, InboundGroupSession, OutboundGroupSession, SenderData,
        VerifyJson,
    },
    session_manager::CollectStrategy,
    store::{BackupDecryptionKey, Changes, CryptoStore, MemoryStore, RoomSettings},
    types::{
        events::{
            room::encrypted::{
                EncryptedEvent, EncryptedToDeviceEvent, RoomEventEncryptionScheme,
                ToDeviceEncryptedEventContent,
            },
            room_key_withheld::{
                MegolmV1AesSha2WithheldContent, RoomKeyWithheldContent, WithheldCode,
            },
            ToDeviceEvent,
        },
        CrossSigningKey, DeviceKeys, EventEncryptionAlgorithm, SignedKey, SigningKeys,
    },
    utilities::json_convert,
    verification::tests::{bob_id, outgoing_request_to_event, request_to_event},
    Account, CryptoStoreError, DeviceData, EncryptionSettings, LocalTrust, MegolmError, OlmError,
    OutgoingRequests, ToDeviceRequest, UserIdentities,
};

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
    let data = response_from_file(&test_json::KEYS_UPLOAD);
    upload_keys::v3::Response::try_from_http_response(data)
        .expect("Can't parse the `/keys/upload` response")
}

fn keys_query_response() -> get_keys::v3::Response {
    let data = response_from_file(&test_json::KEYS_QUERY);
    get_keys::v3::Response::try_from_http_response(data)
        .expect("Can't parse the `/keys/upload` response")
}

pub fn to_device_requests_to_content(
    requests: Vec<Arc<ToDeviceRequest>>,
) -> ToDeviceEncryptedEventContent {
    let to_device_request = &requests[0];

    to_device_request
        .messages
        .values()
        .next()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .deserialize_as()
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

    response.one_time_key_counts.insert(DeviceKeyAlgorithm::SignedCurve25519, uint!(50));

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

    assert!(machine
        .inner
        .group_session_manager
        .get_outbound_group_session(room_id)
        .unwrap()
        .invalidated());
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
        .deserialize_as()
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

    let key_counts = BTreeMap::from([(DeviceKeyAlgorithm::SignedCurve25519, 49u8.into())]);
    machine
        .receive_sync_changes(EncryptionSyncChanges {
            to_device_events: Vec::new(),
            changed_devices: &Default::default(),
            one_time_keys_counts: &key_counts,
            unused_fallback_keys: None,
            next_batch_token: None,
        })
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
        .deserialize_as()
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
            DeviceKeyAlgorithm::SignedCurve25519,
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
async fn test_session_creation() {
    let (alice_machine, bob_machine, mut one_time_keys) =
        get_machine_pair(alice_id(), user_id(), false).await;
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
        get_machine_pair(alice_id(), user_id(), false).await;
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

async fn olm_encryption_test_helper(use_fallback_key: bool) {
    let (alice, bob) = get_machine_pair_with_session(alice_id(), user_id(), use_fallback_key).await;

    let bob_device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

    let (_, content) = bob_device
        .encrypt("m.dummy", ToDeviceDummyEventContent::new())
        .await
        .expect("We should be able to encrypt a dummy event.");

    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        content.deserialize_as().expect("We should be able to deserialize the encrypted content"),
    );

    // Decrypting the first time should succeed.
    let decrypted = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
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
            let res = bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
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

#[async_test]
async fn test_room_key_sharing() {
    let (alice, bob) = get_machine_pair_with_session(alice_id(), user_id(), false).await;

    let room_id = room_id!("!test:example.org");

    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
        .await
        .unwrap();

    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        to_device_requests_to_content(to_device_requests),
    );
    let event = json_convert(&event).unwrap();

    let alice_session =
        alice.inner.group_session_manager.get_outbound_group_session(room_id).unwrap();

    let (decrypted, room_key_updates) = bob
        .receive_sync_changes(EncryptionSyncChanges {
            to_device_events: vec![event],
            changed_devices: &Default::default(),
            one_time_keys_counts: &Default::default(),
            unused_fallback_keys: None,
            next_batch_token: None,
        })
        .await
        .unwrap();

    let event = decrypted[0].deserialize().unwrap();

    if let AnyToDeviceEvent::RoomKey(event) = event {
        assert_eq!(&event.sender, alice.user_id());
        assert!(event.content.session_key.is_empty());
    } else {
        panic!("expected RoomKeyEvent found {event:?}");
    }

    let session = bob.store().get_inbound_group_session(room_id, alice_session.session_id()).await;

    assert!(session.unwrap().is_some());

    assert_eq!(room_key_updates.len(), 1);
    assert_eq!(room_key_updates[0].room_id, room_id);
    assert_eq!(room_key_updates[0].session_id, alice_session.session_id());
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
            OutgoingRequests::ToDeviceRequest(request) => {
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
            OutgoingRequests::ToDeviceRequest(request) => {
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

    let group_session = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
            Ok((tr, res))
        })
        .await
        .unwrap()
        .inbound_group_session
        .unwrap();
    bob.store().save_inbound_group_sessions(&[group_session.clone()]).await.unwrap();

    // when we decrypt the room key, the
    // inbound_group_session_streamroom_keys_received_stream should tell us
    // about it.
    let room_keys = room_keys_received_stream
        .next()
        .now_or_never()
        .flatten()
        .expect("We should have received an update of room key infos");
    assert_eq!(room_keys.len(), 1);
    assert_eq!(room_keys[0].session_id, group_session.session_id());

    let plaintext = "It is a secret to everybody";

    let content = RoomMessageEventContent::text_plain(plaintext);

    let encrypted_content = alice
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
        .await
        .unwrap();

    let event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": encrypted_content,
    });

    let event = json_convert(&event).unwrap();

    let decrypted_event =
        bob.decrypt_room_event(&event, room_id).await.unwrap().event.deserialize().unwrap();

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

#[async_test]
async fn test_withheld_unverified() {
    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;
    let room_id = room_id!("!test:example.org");

    let room_keys_withheld_received_stream = bob.store().room_keys_withheld_received_stream();
    pin_mut!(room_keys_withheld_received_stream);

    let encryption_settings = EncryptionSettings::default();
    let encryption_settings = EncryptionSettings {
        sharing_strategy: CollectStrategy::new_device_based(true),
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
        .deserialize_as::<RoomKeyWithheldContent>()
        .expect("Deserialize should work");

    let event = ToDeviceEvent::new(alice.user_id().to_owned(), wh_content);

    let event = json_convert(&event).unwrap();

    bob.receive_sync_changes(EncryptionSyncChanges {
        to_device_events: vec![event],
        changed_devices: &Default::default(),
        one_time_keys_counts: &Default::default(),
        unused_fallback_keys: None,
        next_batch_token: None,
    })
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

    let content = alice
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
        .await
        .unwrap();

    let room_event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": content,
    });
    let room_event = json_convert(&room_event).unwrap();

    let decrypt_result = bob.decrypt_room_event(&room_event, room_id).await;

    assert_matches!(&decrypt_result, Err(MegolmError::MissingRoomKey(Some(_))));

    let err = decrypt_result.err().unwrap();
    assert_matches!(err, MegolmError::MissingRoomKey(Some(WithheldCode::Unverified)));
}

#[async_test]
async fn test_decryption_verification_state() {
    macro_rules! assert_shield {
        ($foo: ident, $strict: ident, $lax: ident) => {
            let lax = $foo.verification_state.to_shield_state_lax();
            let strict = $foo.verification_state.to_shield_state_strict();

            assert_matches!(lax, ShieldState::$lax { .. });
            assert_matches!(strict, ShieldState::$strict { .. });
        };
    }
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

    let group_session = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
            Ok((tr, res))
        })
        .await
        .unwrap()
        .inbound_group_session;

    let export = group_session.as_ref().unwrap().clone().export().await;

    bob.store().save_inbound_group_sessions(&[group_session.unwrap()]).await.unwrap();

    let plaintext = "It is a secret to everybody";

    let content = RoomMessageEventContent::text_plain(plaintext);

    let encrypted_content = alice
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
        .await
        .unwrap();

    let event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": encrypted_content,
    });

    let event = json_convert(&event).unwrap();

    let encryption_info =
        bob.decrypt_room_event(&event, room_id).await.unwrap().encryption_info.unwrap();

    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnsignedDevice),
        encryption_info.verification_state
    );

    assert_shield!(encryption_info, Red, Red);

    // get_room_event_encryption_info should return the same information
    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();
    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnsignedDevice),
        encryption_info.verification_state
    );
    assert_shield!(encryption_info, Red, Red);

    // Local trust state has no effect
    bob.get_device(alice.user_id(), alice_device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .set_trust_state(LocalTrust::Verified);

    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnsignedDevice),
        encryption_info.verification_state
    );
    assert_shield!(encryption_info, Red, Red);

    setup_cross_signing_for_machine_test_helper(&alice, &bob).await;
    let bob_id_from_alice = alice.get_identity(bob.user_id(), None).await.unwrap();
    assert_matches!(bob_id_from_alice, Some(UserIdentities::Other(_)));
    let alice_id_from_bob = bob.get_identity(alice.user_id(), None).await.unwrap();
    assert_matches!(alice_id_from_bob, Some(UserIdentities::Other(_)));

    // we setup cross signing but nothing is signed yet
    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnsignedDevice),
        encryption_info.verification_state
    );
    assert_shield!(encryption_info, Red, Red);

    // Let alice sign her device
    sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity),
        encryption_info.verification_state
    );

    assert_shield!(encryption_info, Red, None);

    // Given alice is verified
    mark_alice_identity_as_verified_test_helper(&alice, &bob).await;

    // Update bob's store to make sure there is no useful SenderData for this
    // session.
    let mut session = load_session(&bob, room_id, &event).await.unwrap().unwrap();
    session.sender_data = SenderData::unknown();
    save_session(&bob, session).await.unwrap();

    // When I get the encryption info
    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();
    // Then it should say Verified
    assert_eq!(VerificationState::Verified, encryption_info.verification_state);
    assert_shield!(encryption_info, None, None);

    // And the updated SenderData should have been saved into the store.
    let session = load_session(&bob, room_id, &event).await.unwrap().unwrap();
    assert_let!(SenderData::SenderKnown { .. } = session.sender_data);

    // Simulate an imported session, to change verification state
    let imported = InboundGroupSession::from_export(&export).unwrap();
    bob.store().save_inbound_group_sessions(&[imported]).await.unwrap();

    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

    // As soon as the key source is unsafe the verification state (or
    // existence) of the device is meaningless
    assert_eq!(
        VerificationState::Unverified(VerificationLevel::None(DeviceLinkProblem::InsecureSource)),
        encryption_info.verification_state
    );

    assert_shield!(encryption_info, Red, Grey);
}

async fn load_session(
    machine: &OlmMachine,
    room_id: &RoomId,
    event: &Raw<EncryptedEvent>,
) -> Result<Option<InboundGroupSession>, CryptoStoreError> {
    let event = event.deserialize().unwrap();
    let session_id = match &event.content.scheme {
        RoomEventEncryptionScheme::MegolmV1AesSha2(s) => &s.session_id,
        #[cfg(feature = "experimental-algorithms")]
        RoomEventEncryptionScheme::MegolmV2AesSha2(s) => &s.session_id,
        RoomEventEncryptionScheme::Unknown(_) => {
            panic!("Unknown encryption scheme - can't find session ID!")
        }
    };

    machine.store().get_inbound_group_session(room_id, session_id).await
}

async fn save_session(
    machine: &OlmMachine,
    session: InboundGroupSession,
) -> Result<(), CryptoStoreError> {
    machine.store().save_inbound_group_sessions(&[session]).await
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
    assert_matches!(bob.decrypt_room_event(&event, room_id).await, Err(MegolmError::JsonError(..)));

    // so should get_room_event_encryption_info
    assert_matches!(
        bob.get_room_event_encryption_info(&event, room_id).await,
        Err(MegolmError::JsonError(..))
    );
}

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

    let kq_response = KeyQueryResponse::try_from_http_response(response_from_file(&json))
        .expect("Can't parse the `/keys/upload` response");

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

    let kq_response = KeyQueryResponse::try_from_http_response(response_from_file(&json))
        .expect("Can't parse the `/keys/upload` response");

    alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
}

pub async fn mark_alice_identity_as_verified_test_helper(alice: &OlmMachine, bob: &OlmMachine) {
    let alice_device =
        bob.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();

    let alice_identity =
        bob.get_identity(alice.user_id(), None).await.unwrap().unwrap().other().unwrap();
    let upload_request = alice_identity.verify().await.unwrap();

    let raw_extracted =
        upload_request.signed_keys.get(alice.user_id()).unwrap().iter().next().unwrap().1.get();

    let new_signature: CrossSigningKey = serde_json::from_str(raw_extracted).unwrap();

    let user_key_id = bob
        .bootstrap_cross_signing(false)
        .await
        .expect("Expect Alice x-signing key request")
        .upload_signing_keys_req
        .user_signing_key
        .unwrap()
        .get_first_key_and_id()
        .unwrap()
        .0
        .to_owned();

    // add the new signature to alice msk
    let mut alice_updated_msk =
        alice_device.device_owner_identity.as_ref().unwrap().master_key().as_ref().to_owned();

    alice_updated_msk.signatures.add_signature(
        bob.user_id().to_owned(),
        user_key_id.to_owned(),
        new_signature.signatures.get_signature(bob.user_id(), &user_key_id).unwrap(),
    );

    let alice_x_keys = alice
        .bootstrap_cross_signing(false)
        .await
        .expect("Expect Alice x-signing key request")
        .upload_signing_keys_req;

    let json = json!({
        "device_keys": {
            alice.user_id() : { alice.device_id():  alice_device.as_device_keys().to_owned() }
        },
        "failures": {},
        "master_keys": {
            alice.user_id() : alice_updated_msk,
        },
        "user_signing_keys": {
            alice.user_id() : alice_x_keys.user_signing_key.unwrap(),
        },
        "self_signing_keys": {
            alice.user_id() : alice_x_keys.self_signing_key.unwrap(),
        },
      }
    );

    let kq_response = KeyQueryResponse::try_from_http_response(response_from_file(&json))
        .expect("Can't parse the `/keys/upload` response");

    alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();

    // so alice identity should be now trusted

    assert!(bob
        .get_identity(alice.user_id(), None)
        .await
        .unwrap()
        .unwrap()
        .other()
        .unwrap()
        .is_verified());
}

#[async_test]
async fn test_verification_states_multiple_device() {
    let (bob, _) = get_prepared_machine_test_helper(user_id(), false).await;

    let other_user_id = user_id!("@web2:localhost:8482");

    let data = response_from_file(&test_json::KEYS_QUERY_TWO_DEVICES_ONE_SIGNED);
    let response = get_keys::v3::Response::try_from_http_response(data)
        .expect("Can't parse the `/keys/upload` response");

    let (device_change, identity_change) =
        bob.receive_keys_query_response(&TransactionId::new(), &response).await.unwrap();
    assert_eq!(device_change.new.len(), 2);
    assert_eq!(identity_change.new.len(), 1);
    //
    let devices = bob.store().get_user_devices(other_user_id).await.unwrap();
    assert_eq!(devices.devices().count(), 2);

    let fake_room_id = room_id!("!roomid:example.com");

    // We just need a fake session to export it
    // We will use the export to create various inbounds with other claimed
    // ownership
    let id_keys = bob.identity_keys();
    let fake_device_id = bob.device_id().into();
    let olm = OutboundGroupSession::new(
        fake_device_id,
        Arc::new(id_keys),
        fake_room_id,
        EncryptionSettings::default(),
    )
    .unwrap()
    .session_key()
    .await;

    let web_unverified_inbound_session = InboundGroupSession::new(
        Curve25519PublicKey::from_base64("LTpv2DGMhggPAXO02+7f68CNEp6A40F0Yl8B094Y8gc").unwrap(),
        Ed25519PublicKey::from_base64("loz5i40dP+azDtWvsD0L/xpnCjNkmrcvtXVXzCHX8Vw").unwrap(),
        fake_room_id,
        &olm,
        SenderData::unknown(),
        EventEncryptionAlgorithm::MegolmV1AesSha2,
        None,
    )
    .unwrap();

    let (state, _) = bob
        .get_or_update_verification_state(&web_unverified_inbound_session, other_user_id)
        .await
        .unwrap();
    assert_eq!(VerificationState::Unverified(VerificationLevel::UnsignedDevice), state);

    let web_signed_inbound_session = InboundGroupSession::new(
        Curve25519PublicKey::from_base64("XJixbpnfIk+RqcK5T6moqVY9d9Q1veR8WjjSlNiQNT0").unwrap(),
        Ed25519PublicKey::from_base64("48f3WQAMGwYLBg5M5qUhqnEVA8yeibjZpPsShoWMFT8").unwrap(),
        fake_room_id,
        &olm,
        SenderData::unknown(),
        EventEncryptionAlgorithm::MegolmV1AesSha2,
        None,
    )
    .unwrap();

    let (state, _) = bob
        .get_or_update_verification_state(&web_signed_inbound_session, other_user_id)
        .await
        .unwrap();

    assert_eq!(VerificationState::Unverified(VerificationLevel::UnverifiedIdentity), state);
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

    let content = alice
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
        .await
        .unwrap();

    let room_event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": content,
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

    let group_session = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
            Ok((tr, res))
        })
        .await
        .unwrap()
        .inbound_group_session;
    bob.store().save_inbound_group_sessions(&[group_session.unwrap()]).await.unwrap();

    let room_event = json_convert(&room_event).unwrap();

    let decrypt_error = bob.decrypt_room_event(&room_event, room_id).await.unwrap_err();

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
async fn test_interactive_verification() {
    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;

    let bob_device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

    assert!(!bob_device.is_verified());

    let (alice_sas, request) = bob_device.start_verification().await.unwrap();

    let event = request_to_event(alice.user_id(), &request.into());
    bob.handle_verification_event(&event).await;

    let bob_sas = bob
        .get_verification(alice.user_id(), alice_sas.flow_id().as_str())
        .unwrap()
        .sas_v1()
        .unwrap();

    assert!(alice_sas.emoji().is_none());
    assert!(bob_sas.emoji().is_none());

    let event = bob_sas.accept().map(|r| request_to_event(bob.user_id(), &r)).unwrap();

    alice.handle_verification_event(&event).await;

    let (event, request_id) = alice
        .inner
        .verification_machine
        .outgoing_messages()
        .first()
        .map(|r| (outgoing_request_to_event(alice.user_id(), r), r.request_id.to_owned()))
        .unwrap();
    alice.mark_request_as_sent(&request_id, &ToDeviceResponse::new()).await.unwrap();
    bob.handle_verification_event(&event).await;

    let (event, request_id) = bob
        .inner
        .verification_machine
        .outgoing_messages()
        .first()
        .map(|r| (outgoing_request_to_event(bob.user_id(), r), r.request_id.to_owned()))
        .unwrap();
    alice.handle_verification_event(&event).await;
    bob.mark_request_as_sent(&request_id, &ToDeviceResponse::new()).await.unwrap();

    assert!(alice_sas.emoji().is_some());
    assert!(bob_sas.emoji().is_some());

    assert_eq!(alice_sas.emoji(), bob_sas.emoji());
    assert_eq!(alice_sas.decimals(), bob_sas.decimals());

    let contents = bob_sas.confirm().await.unwrap().0;
    assert!(contents.len() == 1);
    let event = request_to_event(bob.user_id(), &contents[0]);
    alice.handle_verification_event(&event).await;

    assert!(!alice_sas.is_done());
    assert!(!bob_sas.is_done());

    let contents = alice_sas.confirm().await.unwrap().0;
    assert!(contents.len() == 1);
    let event = request_to_event(alice.user_id(), &contents[0]);

    assert!(alice_sas.is_done());
    assert!(bob_device.is_verified());

    let alice_device =
        bob.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();

    assert!(!alice_device.is_verified());
    bob.handle_verification_event(&event).await;
    assert!(bob_sas.is_done());
    assert!(alice_device.is_verified());
}

#[async_test]
async fn test_interactive_verification_started_from_request() {
    let (alice, bob) =
        get_machine_pair_with_setup_sessions_test_helper(alice_id(), user_id(), false).await;

    // ----------------------------------------------------------------------------
    // On Alice's device:
    let bob_device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

    assert!(!bob_device.is_verified());

    // Alice sends a verification request with her desired methods to Bob
    let (alice_ver_req, request) =
        bob_device.request_verification_with_methods(vec![VerificationMethod::SasV1]);

    // ----------------------------------------------------------------------------
    // On Bobs's device:
    let event = request_to_event(alice.user_id(), &request);
    bob.handle_verification_event(&event).await;
    let flow_id = alice_ver_req.flow_id().as_str();

    let verification_request = bob.get_verification_request(alice.user_id(), flow_id).unwrap();

    // Bob accepts the request, sending a Ready request
    let accept_request =
        verification_request.accept_with_methods(vec![VerificationMethod::SasV1]).unwrap();
    // And also immediately sends a start request
    let (_, start_request_from_bob) = verification_request.start_sas().await.unwrap().unwrap();

    // ----------------------------------------------------------------------------
    // On Alice's device:

    // Alice receives the Ready
    let event = request_to_event(bob.user_id(), &accept_request);
    alice.handle_verification_event(&event).await;

    let verification_request = alice.get_verification_request(bob.user_id(), flow_id).unwrap();

    // And also immediately sends a start request
    let (alice_sas, start_request_from_alice) =
        verification_request.start_sas().await.unwrap().unwrap();

    // Now alice receives Bob's start:
    let event = request_to_event(bob.user_id(), &start_request_from_bob);
    alice.handle_verification_event(&event).await;

    // Since Alice's user id is lexicographically smaller than Bob's, Alice does not
    // do anything with the request, however.
    assert!(alice.user_id() < bob.user_id());

    // ----------------------------------------------------------------------------
    // On Bob's device:

    // Bob receives Alice's start:
    let event = request_to_event(alice.user_id(), &start_request_from_alice);
    bob.handle_verification_event(&event).await;

    let bob_sas = bob
        .get_verification(alice.user_id(), alice_sas.flow_id().as_str())
        .unwrap()
        .sas_v1()
        .unwrap();

    assert!(alice_sas.emoji().is_none());
    assert!(bob_sas.emoji().is_none());

    // ... and accepts it
    let event = bob_sas.accept().map(|r| request_to_event(bob.user_id(), &r)).unwrap();

    // ----------------------------------------------------------------------------
    // On Alice's device:

    // Alice receives the Accept request:
    alice.handle_verification_event(&event).await;

    // Alice sends a key
    let msgs = alice.inner.verification_machine.outgoing_messages();
    assert!(msgs.len() == 1);
    let msg = &msgs[0];
    let event = outgoing_request_to_event(alice.user_id(), msg);
    alice.inner.verification_machine.mark_request_as_sent(&msg.request_id);

    // ----------------------------------------------------------------------------
    // On Bob's device:

    // And bob receive's it:
    bob.handle_verification_event(&event).await;

    // Now bob sends a key
    let msgs = bob.inner.verification_machine.outgoing_messages();
    assert!(msgs.len() == 1);
    let msg = &msgs[0];
    let event = outgoing_request_to_event(bob.user_id(), msg);
    bob.inner.verification_machine.mark_request_as_sent(&msg.request_id);

    // ----------------------------------------------------------------------------
    // On Alice's device:

    // And alice receives it
    alice.handle_verification_event(&event).await;

    // As a result, both devices now can show emojis/decimals
    assert!(alice_sas.emoji().is_some());
    assert!(bob_sas.emoji().is_some());

    // ----------------------------------------------------------------------------
    // On Bob's device:

    assert_eq!(alice_sas.emoji(), bob_sas.emoji());
    assert_eq!(alice_sas.decimals(), bob_sas.decimals());

    // Bob first confirms that the emojis match and sends the MAC...
    let contents = bob_sas.confirm().await.unwrap().0;
    assert!(contents.len() == 1);
    let event = request_to_event(bob.user_id(), &contents[0]);

    // ----------------------------------------------------------------------------
    // On Alice's device:

    // ...which alice receives
    alice.handle_verification_event(&event).await;

    assert!(!alice_sas.is_done());
    assert!(!bob_sas.is_done());

    // Now alice confirms that the emojis match and sends...
    let contents = alice_sas.confirm().await.unwrap().0;
    assert!(contents.len() == 2);
    // ... her own MAC...
    let event_mac = request_to_event(alice.user_id(), &contents[0]);
    // ... and a Done message
    let event_done = request_to_event(alice.user_id(), &contents[1]);

    // ----------------------------------------------------------------------------
    // On Bob's device:

    // Bob receives the MAC message
    bob.handle_verification_event(&event_mac).await;

    // Bob verifies that the MAC is valid and also sends a "done" message.
    let msgs = bob.inner.verification_machine.outgoing_messages();
    eprintln!("{msgs:?}");
    assert!(msgs.len() == 1);
    let event = msgs.first().map(|r| outgoing_request_to_event(bob.user_id(), r)).unwrap();

    let alice_device =
        bob.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();

    assert!(!bob_sas.is_done());
    assert!(!alice_device.is_verified());
    // And Bob receives the Done message of alice.
    bob.handle_verification_event(&event_done).await;

    assert!(bob_sas.is_done());
    assert!(alice_device.is_verified());

    // ----------------------------------------------------------------------------
    // On Alice's device:

    assert!(!alice_sas.is_done());
    assert!(!bob_device.is_verified());
    // Alices receives the done message
    eprintln!("{event:?}");
    alice.handle_verification_event(&event).await;

    assert!(alice_sas.is_done());
    assert!(bob_device.is_verified());
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

    let _ = bob
        .receive_sync_changes(EncryptionSyncChanges {
            to_device_events: vec![event],
            changed_devices: &changed_devices,
            one_time_keys_counts: &key_counts,
            unused_fallback_keys: None,
            next_batch_token: None,
        })
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

    let encrypted_content =
        alice.encrypt_room_event_raw(room_id, "m.room_key", &content).await.unwrap();
    let event = json!({
        "sender": alice.user_id(),
        "content": encrypted_content,
        "type": "m.room.encrypted",
    });

    let event: EncryptedToDeviceEvent = serde_json::from_value(event).unwrap();

    let decrypt_result = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
            Ok((tr, res))
        })
        .await;

    assert_matches!(decrypt_result, Err(OlmError::EventError(EventError::UnsupportedAlgorithm)));

    let event: Raw<AnyToDeviceEvent> = json_convert(&event).unwrap();

    bob.receive_sync_changes(EncryptionSyncChanges {
        to_device_events: vec![event],
        changed_devices: &changed_devices,
        one_time_keys_counts: &key_counts,
        unused_fallback_keys: None,
        next_batch_token: None,
    })
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
    let content = outbound.encrypt("m.dummy", &content).await;
    alice.store().save_inbound_group_sessions(&[inbound]).await.unwrap();

    let event = json!({
        "sender": alice.user_id(),
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "type": "m.room.encrypted",
        "content": content,
    });
    let event = json_convert(&event).unwrap();

    assert_matches!(
        alice.decrypt_room_event(&event, room_id).await,
        Err(MegolmError::MismatchedIdentityKeys { .. })
    );
}

#[async_test]
async fn importing_private_cross_signing_keys_verifies_the_public_identity() {
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
    let wait = tokio::spawn(async move {
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
async fn room_settings_returns_none_for_unknown_room() {
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;
    let settings = machine.room_settings(room_id!("!test2:localhost")).await.unwrap();
    assert!(settings.is_none());
}

#[async_test]
async fn stores_and_returns_room_settings() {
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;
    let room_id = room_id!("!test:localhost");

    let settings = RoomSettings {
        algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
        only_allow_trusted_devices: true,
        session_rotation_period: Some(Duration::from_secs(10)),
        session_rotation_period_messages: Some(1234),
    };

    machine.set_room_settings(room_id, &settings).await.unwrap();
    assert_eq!(machine.room_settings(room_id).await.unwrap(), Some(settings));
}

#[async_test]
async fn set_room_settings_rejects_invalid_algorithms() {
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;
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
async fn set_room_settings_rejects_changes() {
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;
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
async fn set_room_settings_accepts_noop_changes() {
    let machine = OlmMachine::new(user_id(), alice_device_id()).await;
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

#[async_test]
async fn test_send_encrypted_to_device() {
    let (alice, bob) = get_machine_pair_with_session(alice_id(), user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();
    let raw_encrypted = device
        .encrypt_event_raw(custom_event_type, &custom_content)
        .await
        .expect("Should have encryted the content");

    let request = ToDeviceRequest::new(
        bob.user_id(),
        DeviceIdOrAllDevices::DeviceId(bob_device_id().to_owned()),
        "m.room.encrypted",
        raw_encrypted.cast(),
    );

    assert_eq!("m.room.encrypted", request.event_type.to_string());

    let messages = &request.messages;
    assert_eq!(1, messages.len());
    assert!(messages.get(bob.user_id()).is_some());
    let target_devices = messages.get(bob.user_id()).unwrap();
    assert_eq!(1, target_devices.len());
    assert!(target_devices
        .get(&DeviceIdOrAllDevices::DeviceId(bob_device_id().to_owned()))
        .is_some());

    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        to_device_requests_to_content(vec![request.clone().into()]),
    );

    let event = json_convert(&event).unwrap();

    let sync_changes = EncryptionSyncChanges {
        to_device_events: vec![event],
        changed_devices: &Default::default(),
        one_time_keys_counts: &Default::default(),
        unused_fallback_keys: None,
        next_batch_token: None,
    };

    let (decrypted, _) = bob.receive_sync_changes(sync_changes).await.unwrap();

    assert_eq!(1, decrypted.len());

    let decrypted_event = decrypted[0].deserialize().unwrap();

    assert_eq!(decrypted_event.event_type().to_string(), custom_event_type.to_owned());

    let decrypted_value = to_raw_value(&decrypted[0]).unwrap();
    let decrypted_value = serde_json::to_value(decrypted_value).unwrap();

    assert_eq!(
        decrypted_value.get("content").unwrap().get("device_id").unwrap().as_str().unwrap(),
        custom_content.get("device_id").unwrap().as_str().unwrap(),
    );

    assert_eq!(
        decrypted_value.get("content").unwrap().get("rooms").unwrap().as_array().unwrap(),
        custom_content.get("rooms").unwrap().as_array().unwrap(),
    );
}

#[async_test]
async fn test_send_encrypted_to_device_no_session() {
    let (alice, bob, _) = get_machine_pair(alice_id(), user_id(), false).await;

    let custom_event_type = "m.new_device";

    let custom_content = json!({
            "device_id": "XYZABCDE",
            "rooms": ["!726s6s6q:example.com"]
    });

    let encryption_result = alice
        .get_device(bob.user_id(), bob_device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .encrypt_event_raw(custom_event_type, &custom_content)
        .await;

    assert_matches!(encryption_result, Err(OlmError::MissingSession));
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

    // Save the first room key.
    let group_session = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob
                .decrypt_to_device_event(&mut tr, &first_room_key_event, &mut Changes::default())
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
    let first_message_encrypted_content =
        alice.encrypt_room_event(room_id, first_message_content).await.unwrap();

    let mut first_message_encrypted_event = json!({
        "event_id": "$message1",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": first_message_encrypted_content,
    });
    let raw_encrypted_event = json_convert(&first_message_encrypted_event).unwrap();

    // Bob has the room key, so first message should be decrypted successfully.
    let raw_decrypted_event = bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

    let decrypted_event = raw_decrypted_event.event.deserialize().unwrap();
    assert_matches!(
        decrypted_event,
        AnyTimelineEvent::MessageLike(AnyMessageLikeEvent::RoomMessage(first_message))
    );

    let first_message = first_message.as_original().unwrap();
    assert_eq!(first_message.content.body(), first_message_text);
    assert!(first_message.unsigned.relations.is_empty());

    assert!(raw_decrypted_event.encryption_info.is_some());
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
    let second_message_content = RoomMessageEventContent::text_plain(second_message_text)
        .make_replacement(first_message, None);
    let second_message_encrypted_content =
        alice.encrypt_room_event(room_id, second_message_content).await.unwrap();

    let second_message_encrypted_event = json!({
        "event_id": "$message2",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": second_message_encrypted_content,
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
    let raw_decrypted_event = bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

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

    assert!(raw_decrypted_event.encryption_info.is_some());
    let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
    assert_eq!(unsigned_encryption_info.len(), 1);
    let replace_encryption_result =
        unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
    assert_matches!(
        replace_encryption_result,
        UnsignedDecryptionResult::UnableToDecrypt(UnableToDecryptInfo {
            session_id: Some(second_room_key_session_id)
        })
    );

    // Give Bob the second room key.
    let group_session = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob
                .decrypt_to_device_event(&mut tr, &second_room_key_event, &mut Changes::default())
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
    let raw_decrypted_event = bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

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

    assert!(raw_decrypted_event.encryption_info.is_some());
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
    let third_message_encrypted_content =
        alice.encrypt_room_event(room_id, third_message_content).await.unwrap();

    let third_message_encrypted_event = json!({
        "event_id": "$message3",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": third_message_encrypted_content,
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
    let raw_decrypted_event = bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

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
    assert_matches!(thread.latest_event.deserialize(), Ok(AnyMessageLikeEvent::RoomEncrypted(_)));

    assert!(raw_decrypted_event.encryption_info.is_some());
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
            session_id: Some(third_room_key_session_id)
        })
    );

    // Give Bob the third room key.
    let group_session = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob
                .decrypt_to_device_event(&mut tr, &third_room_key_event, &mut Changes::default())
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
    let raw_decrypted_event = bob.decrypt_room_event(&raw_encrypted_event, room_id).await.unwrap();

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
        Ok(AnyMessageLikeEvent::RoomMessage(third_message))
    );
    let third_message = third_message.as_original().unwrap();
    assert_eq!(third_message.content.body(), third_message_text);

    assert!(raw_decrypted_event.encryption_info.is_some());
    let unsigned_encryption_info = raw_decrypted_event.unsigned_encryption_info.unwrap();
    assert_eq!(unsigned_encryption_info.len(), 2);
    let replace_encryption_result =
        unsigned_encryption_info.get(&UnsignedEventLocation::RelationsReplace).unwrap();
    assert_matches!(replace_encryption_result, UnsignedDecryptionResult::Decrypted(_));
    let thread_encryption_result =
        unsigned_encryption_info.get(&UnsignedEventLocation::RelationsThreadLatestEvent).unwrap();
    assert_matches!(thread_encryption_result, UnsignedDecryptionResult::Decrypted(_));
}
