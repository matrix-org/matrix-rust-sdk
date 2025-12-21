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

//! A set of helper functions for creating [`OlmMachine`]s, and pairs of
//! interconnected machines.

use std::{collections::BTreeMap, ops::Deref, sync::Arc};

use as_variant::as_variant;
use matrix_sdk_common::deserialized_responses::ProcessedToDeviceEvent;
use matrix_sdk_test::{ruma_response_from_json, test_json};
use ruma::{
    DeviceId, OwnedOneTimeKeyId, TransactionId, UserId,
    api::client::keys::{
        claim_keys,
        get_keys::{self, v3::Response as KeysQueryResponse},
        upload_keys,
    },
    device_id,
    encryption::OneTimeKey,
    events::dummy::ToDeviceDummyEventContent,
    serde::Raw,
    user_id,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    Account, CollectStrategy, CrossSigningBootstrapRequests, DecryptionSettings, Device,
    DeviceData, EncryptionSyncChanges, OlmMachine, OtherUserIdentityData, TrustRequirement,
    olm::PrivateCrossSigningIdentity,
    store::{CryptoStoreWrapper, MemoryStore, types::Changes},
    types::{
        DeviceKeys,
        events::{ToDeviceEvent, room::encrypted::ToDeviceEncryptedEventContent},
        requests::AnyOutgoingRequest,
    },
    utilities::json_convert,
    verification::VerificationMachine,
};

/// These keys need to be periodically uploaded to the server.
type OneTimeKeys = BTreeMap<OwnedOneTimeKeyId, Raw<OneTimeKey>>;

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

pub fn keys_query_response() -> get_keys::v3::Response {
    let json = &test_json::KEYS_QUERY;
    ruma_response_from_json(json)
}

pub async fn get_prepared_machine_test_helper(
    user_id: &UserId,
    use_fallback_key: bool,
) -> (OlmMachine, OneTimeKeys) {
    let machine = OlmMachine::new(user_id, bob_device_id()).await;

    let request = machine
        .store()
        .with_transaction(|mut tr| async {
            let account = tr.account().await.unwrap();
            account.generate_fallback_key_if_needed();
            account.update_uploaded_key_count(0);
            account.generate_one_time_keys_if_needed();
            let request =
                machine.keys_for_upload(account).await.expect("Can't prepare initial key upload");
            Ok((tr, request))
        })
        .await
        .unwrap();

    let response = keys_upload_response();
    machine.receive_keys_upload_response(&response).await.unwrap();

    let keys = if use_fallback_key { request.fallback_keys } else { request.one_time_keys };

    (machine, keys)
}

pub async fn get_machine_after_query_test_helper() -> (OlmMachine, OneTimeKeys) {
    let (machine, otk) = get_prepared_machine_test_helper(user_id(), false).await;
    let response = keys_query_response();
    let req_id = TransactionId::new();

    machine.receive_keys_query_response(&req_id, &response).await.unwrap();

    (machine, otk)
}

pub async fn get_machine_pair_using_store(
    alice: &UserId,
    bob: &UserId,
    use_fallback_key: bool,
    alice_store: MemoryStore,
    alice_device_id: &DeviceId,
) -> (OlmMachine, OlmMachine, OneTimeKeys) {
    let (bob, otk) = get_prepared_machine_test_helper(bob, use_fallback_key).await;

    let alice = OlmMachine::with_store(alice, alice_device_id, alice_store, None)
        .await
        .expect("Failed to create OlmMachine from supplied store");

    store_each_others_device_data(&alice, &bob).await;
    (alice, bob, otk)
}

pub async fn get_machine_pair(
    alice: &UserId,
    bob: &UserId,
    use_fallback_key: bool,
) -> (OlmMachine, OlmMachine, OneTimeKeys) {
    let (bob, otk) = get_prepared_machine_test_helper(bob, use_fallback_key).await;

    let alice_device = alice_device_id();
    let alice = OlmMachine::new(alice, alice_device).await;

    store_each_others_device_data(&alice, &bob).await;
    (alice, bob, otk)
}

/// Store alice's device data in bob's store and vice versa
async fn store_each_others_device_data(alice: &OlmMachine, bob: &OlmMachine) {
    let alice_device = DeviceData::from_machine_test_helper(alice).await.unwrap();
    let bob_device = DeviceData::from_machine_test_helper(bob).await.unwrap();
    alice.store().save_device_data(&[bob_device]).await.unwrap();
    bob.store().save_device_data(&[alice_device]).await.unwrap();
}

/// Return a pair of [`OlmMachine`]s, with an olm session created on Alice's
/// side, but with no message yet sent.
///
/// Create Alice's `OlmMachine` using the [`MemoryStore`] provided
pub async fn get_machine_pair_with_session_using_store(
    alice: &UserId,
    bob: &UserId,
    use_fallback_key: bool,
    alice_store: MemoryStore,
    alice_device_id: &DeviceId,
) -> (OlmMachine, OlmMachine) {
    let (alice, bob, one_time_keys) =
        get_machine_pair_using_store(alice, bob, use_fallback_key, alice_store, alice_device_id)
            .await;

    build_session_for_pair(alice, bob, one_time_keys).await
}

/// Return a pair of [`OlmMachine`]s, with an olm session created on Alice's
/// side, but with no message yet sent.
pub async fn get_machine_pair_with_session(
    alice: &UserId,
    bob: &UserId,
    use_fallback_key: bool,
) -> (OlmMachine, OlmMachine) {
    let (alice, bob, one_time_keys) = get_machine_pair(alice, bob, use_fallback_key).await;

    build_session_for_pair(alice, bob, one_time_keys).await
}

pub async fn send_and_receive_encrypted_to_device_test_helper(
    sender: &OlmMachine,
    recipient: &OlmMachine,
    event_type: &str,
    content: &Value,
    decryption_settings: &DecryptionSettings,
) -> ProcessedToDeviceEvent {
    let device =
        sender.get_device(recipient.user_id(), recipient.device_id(), None).await.unwrap().unwrap();

    let raw_encrypted = device
        .encrypt_event_raw(event_type, content, CollectStrategy::AllDevices)
        .await
        .expect("Should have encrypted the content");

    receive_encrypted_to_device_test_helper(
        sender.user_id(),
        recipient,
        decryption_settings,
        raw_encrypted,
    )
    .await
}

pub async fn receive_encrypted_to_device_test_helper(
    sender: &UserId,
    recipient: &OlmMachine,
    decryption_settings: &DecryptionSettings,
    raw_encrypted: Raw<ToDeviceEncryptedEventContent>,
) -> ProcessedToDeviceEvent {
    let event = ToDeviceEvent::new(sender.to_owned(), raw_encrypted);

    let event = json_convert(&event).unwrap();

    let sync_changes = EncryptionSyncChanges {
        to_device_events: vec![event],
        changed_devices: &Default::default(),
        one_time_keys_counts: &Default::default(),
        unused_fallback_keys: None,
        next_batch_token: None,
    };

    let (decrypted, _) =
        recipient.receive_sync_changes(sync_changes, decryption_settings).await.unwrap();

    assert_eq!(1, decrypted.len());
    decrypted[0].clone()
}

/// Encrypt the given event content into the content of an
/// olm-encrypted to-device event, suppressing the `sender_device_keys` field in
/// the encrypted content.
///
/// This is much the same as calling [`Device::encrypt`] on the recipient
/// device, other than the suppression of `sender_device_keys`.
///
/// # Arguments
///
/// * `sender` - The OlmMachine to use to encrypt the event.
/// * `recipient` - The recipient of the encrypted event.
/// * `event_type` - The type of the event to encrypt.
/// * `content` - The content of the event to encrypt.
pub async fn build_encrypted_to_device_content_without_sender_data(
    sender: &OlmMachine,
    recipient_device: &DeviceKeys,
    event_type: &str,
    content: &impl Serialize,
) -> ToDeviceEncryptedEventContent {
    let sender_store = &sender.inner.store;

    let sender_key = recipient_device.curve25519_key().unwrap();
    let sessions = sender_store
        .get_sessions(&sender_key.to_base64())
        .await
        .expect("Could not get most recent session")
        .expect("No olm session found");
    let mut olm_session = sessions.lock().await.first().unwrap().clone();

    let plaintext = serde_json::to_string(&json!({
        "sender": sender.user_id(),
        "sender_device": sender.device_id(),
        "keys": { "ed25519": sender.identity_keys().ed25519.to_base64() },
        "recipient": recipient_device.user_id,
        "recipient_keys": { "ed25519": recipient_device.ed25519_key().unwrap().to_base64() },
        "type": event_type,
        "content": content,
    }))
    .unwrap();

    let ciphertext = olm_session.encrypt_helper(&plaintext).await;
    let content =
        olm_session.build_encrypted_event(ciphertext, None).await.expect("could not encrypt");

    sender_store
        .save_changes(Changes { sessions: vec![olm_session], ..Default::default() })
        .await
        .expect("Could not save session");

    content
}

/// Create a session for the two supplied Olm machines to communicate.
pub async fn build_session_for_pair(
    alice: OlmMachine,
    bob: OlmMachine,
    mut one_time_keys: BTreeMap<
        ruma::OwnedKeyId<ruma::OneTimeKeyAlgorithm, ruma::OneTimeKeyName>,
        Raw<OneTimeKey>,
    >,
) -> (OlmMachine, OlmMachine) {
    let (device_key_id, one_time_key) = one_time_keys.pop_first().unwrap();

    let one_time_keys = BTreeMap::from([(
        bob.user_id().to_owned(),
        BTreeMap::from([(
            bob.device_id().to_owned(),
            BTreeMap::from([(device_key_id, one_time_key)]),
        )]),
    )]);

    let response = claim_keys::v3::Response::new(one_time_keys);
    alice.inner.session_manager.create_sessions(&response).await.unwrap();

    (alice, bob)
}

/// Return a pair of [`OlmMachine`]s, with an olm session (initiated
/// by Alice) established between the two.
pub async fn get_machine_pair_with_setup_sessions_test_helper(
    alice: &UserId,
    bob: &UserId,
    use_fallback_key: bool,
) -> (OlmMachine, OlmMachine) {
    let (alice, bob) = get_machine_pair_with_session(alice, bob, use_fallback_key).await;

    let bob_device = alice.get_device(bob.user_id(), bob.device_id(), None).await.unwrap().unwrap();

    let (session, content) =
        bob_device.encrypt("m.dummy", ToDeviceDummyEventContent::new()).await.unwrap();
    alice.store().save_sessions(&[session]).await.unwrap();

    let event =
        ToDeviceEvent::new(alice.user_id().to_owned(), content.deserialize_as_unchecked().unwrap());

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

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
        .unwrap();

    bob.store().save_sessions(&[decrypted.session.session()]).await.unwrap();

    (alice, bob)
}

/// Create an Olm session from the given machine to the given device
pub async fn create_session(
    machine: &OlmMachine,
    user_id: &UserId,
    device_id: &DeviceId,
    key_id: OwnedOneTimeKeyId,
    one_time_key: Raw<OneTimeKey>,
) {
    let one_time_keys = BTreeMap::from([(
        user_id.to_owned(),
        BTreeMap::from([(device_id.to_owned(), BTreeMap::from([(key_id, one_time_key)]))]),
    )]);

    let response = claim_keys::v3::Response::new(one_time_keys);
    machine.inner.session_manager.create_sessions(&response).await.unwrap();
}

/// Given a set of requests returned by `bootstrap_cross_signing` for one user,
/// return a `/keys/query` response which might be returned to another user/
pub fn bootstrap_requests_to_keys_query_response(
    bootstrap_requests: CrossSigningBootstrapRequests,
) -> KeysQueryResponse {
    let mut kq_response = json!({});

    // If we have a master key, add that to the response
    if let Some(key) = bootstrap_requests.upload_signing_keys_req.master_key {
        let user_id = key.user_id.clone();
        kq_response["master_keys"] = json!({user_id: key});
    }

    // If we have a self-signing key, add that
    if let Some(key) = bootstrap_requests.upload_signing_keys_req.self_signing_key {
        let user_id = key.user_id.clone();
        kq_response["self_signing_keys"] = json!({user_id: key});
    }

    // And if we have a device, add that
    if let Some(dk) = bootstrap_requests
        .upload_keys_req
        .and_then(|req| as_variant!(req.request.as_ref(), AnyOutgoingRequest::KeysUpload).cloned())
        .and_then(|keys_upload_request| keys_upload_request.device_keys)
    {
        let user_id: String = dk.get_field("user_id").unwrap().unwrap();
        let device_id: String = dk.get_field("device_id").unwrap().unwrap();
        kq_response["device_keys"] = json!({user_id: { device_id: dk }});
    }

    ruma_response_from_json(&kq_response)
}

/// Create a [`VerificationMachine`] which won't do any useful verification.
///
/// Helper for [`create_signed_device_of_unverified_user`] and
/// [`create_unsigned_device`].
fn dummy_verification_machine() -> VerificationMachine {
    let account = Account::new(user_id!("@test_user:example.com"));
    VerificationMachine::new(
        account.deref().clone(),
        Arc::new(Mutex::new(PrivateCrossSigningIdentity::new(account.user_id().to_owned()))),
        Arc::new(CryptoStoreWrapper::new(
            account.user_id(),
            account.device_id(),
            MemoryStore::new(),
        )),
    )
}

/// Wrap the given [`DeviceKeys`] into a [`Device`], with no known owner
/// identity.
pub fn create_unsigned_device(device_keys: DeviceKeys) -> Device {
    Device {
        inner: DeviceData::try_from(&device_keys).unwrap(),
        verification_machine: dummy_verification_machine(),
        own_identity: None,
        device_owner_identity: None,
    }
}

/// Sign the given [`DeviceKeys`] with a cross-signing identity, and wrap it up
/// as a [`Device`] with that identity.
pub async fn create_signed_device_of_unverified_user(
    mut device_keys: DeviceKeys,
    device_owner_identity: &PrivateCrossSigningIdentity,
) -> Device {
    {
        let self_signing = device_owner_identity.self_signing_key.lock().await;
        let self_signing = self_signing.as_ref().unwrap();
        self_signing.sign_device(&mut device_keys).unwrap();
    }

    let public_identity = OtherUserIdentityData::from_private(device_owner_identity).await;

    let device = Device {
        inner: DeviceData::try_from(&device_keys).unwrap(),
        verification_machine: dummy_verification_machine(),
        own_identity: None,
        device_owner_identity: Some(public_identity.into()),
    };
    assert!(device.is_cross_signed_by_owner());
    device
}

/// Sign the given [`DeviceKeys`] with a cross-signing identity, then sign the
/// identity with our own identity, and wrap both up as a [`Device`].
pub async fn create_signed_device_of_verified_user(
    mut device_keys: DeviceKeys,
    device_owner_identity: &PrivateCrossSigningIdentity,
    own_identity: &PrivateCrossSigningIdentity,
) -> Device {
    // Sign the device keys with the owner's identity.
    {
        let self_signing = device_owner_identity.self_signing_key.lock().await;
        let self_signing = self_signing.as_ref().unwrap();
        self_signing.sign_device(&mut device_keys).unwrap();
    }

    let mut public_identity = OtherUserIdentityData::from_private(device_owner_identity).await;

    // Sign the public identity of the owner with our own identity.
    sign_user_identity_data(own_identity, &mut public_identity).await;

    let device = Device {
        inner: DeviceData::try_from(&device_keys).unwrap(),
        verification_machine: dummy_verification_machine(),
        own_identity: Some(own_identity.to_public_identity().await.unwrap()),
        device_owner_identity: Some(public_identity.into()),
    };
    assert!(device.is_cross_signed_by_owner());
    assert!(device.is_cross_signing_trusted());
    device
}

/// Sign a public user identity with our own user-signing key.
pub async fn sign_user_identity_data(
    signer_private_identity: &PrivateCrossSigningIdentity,
    other_user_identity: &mut OtherUserIdentityData,
) {
    let user_signing = signer_private_identity.user_signing_key.lock().await;

    let user_signing = user_signing.as_ref().unwrap();
    let master = user_signing.sign_user(&*other_user_identity).unwrap();
    other_user_identity.master_key = Arc::new(master.try_into().unwrap());

    user_signing.public_key().verify_master_key(other_user_identity.master_key()).unwrap();
}
