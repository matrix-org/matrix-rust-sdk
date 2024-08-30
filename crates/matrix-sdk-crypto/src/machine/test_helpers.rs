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

use std::collections::BTreeMap;

use as_variant::as_variant;
use matrix_sdk_test::{ruma_response_from_json, test_json};
use ruma::{
    api::client::keys::{
        claim_keys, get_keys, get_keys::v3::Response as KeysQueryResponse, upload_keys,
    },
    device_id,
    encryption::OneTimeKey,
    events::dummy::ToDeviceDummyEventContent,
    serde::Raw,
    user_id, DeviceId, OwnedDeviceKeyId, TransactionId, UserId,
};
use serde_json::json;

use crate::{
    store::Changes, types::events::ToDeviceEvent, CrossSigningBootstrapRequests, DeviceData,
    OlmMachine, OutgoingRequests,
};

/// These keys need to be periodically uploaded to the server.
type OneTimeKeys = BTreeMap<OwnedDeviceKeyId, Raw<OneTimeKey>>;

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

pub async fn get_machine_pair(
    alice: &UserId,
    bob: &UserId,
    use_fallback_key: bool,
) -> (OlmMachine, OlmMachine, OneTimeKeys) {
    let (bob, otk) = get_prepared_machine_test_helper(bob, use_fallback_key).await;

    let alice_device = alice_device_id();
    let alice = OlmMachine::new(alice, alice_device).await;

    let alice_device = DeviceData::from_machine_test_helper(&alice).await.unwrap();
    let bob_device = DeviceData::from_machine_test_helper(&bob).await.unwrap();
    alice.store().save_device_data(&[bob_device]).await.unwrap();
    bob.store().save_device_data(&[alice_device]).await.unwrap();

    (alice, bob, otk)
}

/// Return a pair of [`OlmMachine`]s, with an olm session created on Alice's
/// side, but with no message yet sent.
pub async fn get_machine_pair_with_session(
    alice: &UserId,
    bob: &UserId,
    use_fallback_key: bool,
) -> (OlmMachine, OlmMachine) {
    let (alice, bob, mut one_time_keys) = get_machine_pair(alice, bob, use_fallback_key).await;

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

    let event = ToDeviceEvent::new(alice.user_id().to_owned(), content.deserialize_as().unwrap());

    let decrypted = bob
        .store()
        .with_transaction(|mut tr| async {
            let res = bob.decrypt_to_device_event(&mut tr, &event, &mut Changes::default()).await?;
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
    key_id: OwnedDeviceKeyId,
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
        .and_then(|req| as_variant!(req.request.as_ref(), OutgoingRequests::KeysUpload).cloned())
        .and_then(|keys_upload_request| keys_upload_request.device_keys)
    {
        let user_id: String = dk.get_field("user_id").unwrap().unwrap();
        let device_id: String = dk.get_field("device_id").unwrap().unwrap();
        kq_response["device_keys"] = json!({user_id: { device_id: dk }});
    }

    ruma_response_from_json(&kq_response)
}
