// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Helpers to mock a server that supports the main crypto API and have a client
//! automatically connected to that server, for the purpose of integration
//! tests.
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex,
    },
};

use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::SyncResponseBuilder;
use ruma::{
    api::{client::keys::upload_signatures::v3::SignedKeys, MatrixVersion},
    encryption::{CrossSigningKey, DeviceKeys, OneTimeKey},
    owned_device_id, owned_user_id,
    serde::Raw,
    CrossSigningKeyId, DeviceId, OneTimeKeyAlgorithm, OwnedDeviceId, OwnedOneTimeKeyId,
    OwnedUserId, UserId,
};
use serde::Serialize;
use serde_json::json;
use wiremock::{
    matchers::{method, path, query_param, query_param_is_missing},
    Mock, MockGuard, MockServer, Request, ResponseTemplate,
};

use crate::{authentication::matrix::MatrixSession, config::RequestConfig, Client, SessionTokens};

#[derive(Debug, Default)]
struct Keys {
    device: BTreeMap<OwnedUserId, BTreeMap<String, Raw<DeviceKeys>>>,
    master: BTreeMap<OwnedUserId, Raw<CrossSigningKey>>,
    self_signing: BTreeMap<OwnedUserId, Raw<CrossSigningKey>>,
    user_signing: BTreeMap<OwnedUserId, Raw<CrossSigningKey>>,
    one_time_keys: BTreeMap<
        OwnedUserId,
        BTreeMap<OwnedDeviceId, BTreeMap<OwnedOneTimeKeyId, Raw<OneTimeKey>>>,
    >,
}

/// A [`wiremock`] [`MockServer`] along with useful methods to help mocking
/// matrix crypto API and perform integration test with encryption.
///
/// It implements mock endpoints for the `keys/upload`, will store the uploaded
/// devices and serves them back for incoming `keys/query`. It is also storing
/// and claiming one-time-keys, allowing to set up working olm sessions.
///
/// Adds some helpers like `exhaust_one_time_keys` that allows to simulate a
/// client running out of otks. More can be added if needed later.
///
/// It works like this:
/// * Start by creating the mock server like this [`MatrixKeysMockServer::new`].
///   It will setup the mocks
/// * Create your test client using [`MatrixKeysMockServer::create_client`],
///   this is important as it will set up an access token that will allow to
///   know what client is doing what request.
///
/// # Examples
///
/// ```
/// # tokio_test::block_on(async {
/// use matrix_sdk::{
///     ruma::{device_id, user_id},
///     test_utils::mocks::encryption::MatrixKeysMockServer,
/// };
/// let mock_server = MatrixKeysMockServer::new().await;
///
/// let (alice, bob) = mock_server.set_up_alice_and_bob_for_encryption().await;
///
/// let alice_bob_device = alice
///     .encryption()
///     .get_device(bob.user_id().unwrap(), bob.device_id().unwrap())
///     .await
///     .unwrap()
///     .expect("alice sees bob's device");
///
/// # anyhow::Ok(()) });
/// ```
pub struct MatrixKeysMockServer {
    /// The underlying [`wiremock`] [`MockServer`]
    pub server: MockServer,
    keys: Arc<Mutex<Keys>>,
    token_to_user_id_map: Arc<Mutex<BTreeMap<String, OwnedUserId>>>,
    token_counter: AtomicU32,
}

impl MatrixKeysMockServer {
    /// Creates a new `MatrixKeysMockServer` and mocks the crypto API end-points
    pub async fn new() -> Self {
        let server = MockServer::start().await;
        let keys: Arc<Mutex<Keys>> = Default::default();
        let mock_keys_server = Self {
            server,
            keys,
            token_to_user_id_map: Default::default(),
            token_counter: Default::default(),
        };
        mock_keys_server.mock_endpoints().await;
        mock_keys_server
    }

    /// Creates a `Client` that will use this mock server.
    pub async fn create_client(&self, user_id: &UserId, device_id: &DeviceId) -> Client {
        let client = Client::builder()
            .homeserver_url(self.server.uri())
            .server_versions([MatrixVersion::V1_0])
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap();

        // Create an access token and store the token to user_id mapping
        let next = self.token_counter.fetch_add(1, Ordering::Relaxed);
        let access_token = format!("TOKEN_{}", next);

        {
            let mut mappings = self.token_to_user_id_map.lock().unwrap();
            let auth_string = format!("Bearer {}", access_token);
            mappings.insert(auth_string, user_id.to_owned());
        }

        client
            .restore_session(MatrixSession {
                meta: SessionMeta { user_id: user_id.to_owned(), device_id: device_id.to_owned() },
                tokens: SessionTokens { access_token, refresh_token: None },
            })
            .await
            .unwrap();

        client
    }

    /// Makes the server forget about all the one-time-keys for that device.
    pub fn exhaust_one_time_keys(&self, user_id: OwnedUserId, device_id: OwnedDeviceId) {
        let mut keys = self.keys.lock().unwrap();
        let known_otks = &mut keys.one_time_keys;
        known_otks.entry(user_id).or_default().entry(device_id).or_default().clear();
    }

    /// Utility to properly setup two clients. These two clients will know about
    /// each others (alice will have downloaded bob device keys).
    pub async fn set_up_alice_and_bob_for_encryption(&self) -> (Client, Client) {
        let alice_user_id = owned_user_id!("@alice:example.org");
        let alice_device_id = owned_device_id!("4L1C3");

        let alice = self.create_client(&alice_user_id, &alice_device_id).await;

        let bob_user_id = owned_user_id!("@bob:example.org");
        let bob_device_id = owned_device_id!("B0B0B0B0B");
        let bob = self.create_client(&bob_user_id, &bob_device_id).await;

        // Have Alice track Bob, so she queries his keys later.
        {
            let alice_olm = alice.olm_machine_for_testing().await;
            let alice_olm = alice_olm.as_ref().unwrap();
            alice_olm.update_tracked_users([bob_user_id.as_ref()]).await.unwrap();
        }

        // Have Alice and Bob upload their signed device keys.
        {
            let mut sync_response_builder = SyncResponseBuilder::new();
            let response_body = sync_response_builder.build_json_sync_response();
            let _scope = mock_sync_scoped(&self.server, response_body, None).await;

            alice
                .sync_once(Default::default())
                .await
                .expect("We should be able to sync with Alice so we upload the device keys");
            bob.sync_once(Default::default()).await.unwrap();
        }

        // Run a sync so we do send outgoing requests, including the /keys/query for
        // getting bob's identity.
        let mut sync_response_builder = SyncResponseBuilder::new();

        {
            let _scope = mock_sync_scoped(
                &self.server,
                sync_response_builder.build_json_sync_response(),
                None,
            )
            .await;
            alice
                .sync_once(Default::default())
                .await
                .expect("We should be able to sync so we get theinitial set of devices");
        }

        (alice, bob)
    }

    async fn mock_endpoints(&self) {
        let keys = &self.keys;
        let token_map = &self.token_to_user_id_map;

        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/keys/query"))
            .respond_with(mock_keys_query(keys.clone()))
            .mount(&self.server)
            .await;

        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/keys/upload"))
            .respond_with(mock_keys_upload(keys.clone(), token_map.clone()))
            .mount(&self.server)
            .await;

        Mock::given(method("POST"))
            .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
            .respond_with(mock_keys_device_signing_upload(keys.clone()))
            .mount(&self.server)
            .await;

        Mock::given(method("POST"))
            .and(path("/_matrix/client/unstable/keys/signatures/upload"))
            .respond_with(mock_keys_signature_upload(keys.clone()))
            .mount(&self.server)
            .await;

        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/keys/claim"))
            .respond_with(mock_keys_claimed_request(keys.clone()))
            .mount(&self.server)
            .await;
    }
}

/// Intercepts a `/keys/query` request and mock its results as returned by an
/// actual homeserver.
///
/// Supports filtering by user id, or no filters at all.
fn mock_keys_query(keys: Arc<Mutex<Keys>>) -> impl Fn(&Request) -> ResponseTemplate {
    move |req| {
        #[derive(Debug, serde::Deserialize)]
        struct Parameters {
            device_keys: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
        }

        let params: Parameters = req.body_json().unwrap();

        let keys = keys.lock().unwrap();
        let mut device_keys = keys.device.clone();
        if !params.device_keys.is_empty() {
            device_keys.retain(|user, key_map| {
                if let Some(devices) = params.device_keys.get(user) {
                    if !devices.is_empty() {
                        key_map.retain(|key_id, _json| {
                            devices.iter().any(|device_id| &device_id.to_string() == key_id)
                        });
                    }
                    true
                } else {
                    false
                }
            })
        }

        let master_keys = keys.master.clone();
        let self_signing_keys = keys.self_signing.clone();
        let user_signing_keys = keys.user_signing.clone();

        ResponseTemplate::new(200).set_body_json(json!({
            "device_keys": device_keys,
            "master_keys": master_keys,
            "self_signing_keys": self_signing_keys,
            "user_signing_keys": user_signing_keys,
        }))
    }
}

/// Intercepts a `/keys/upload` query and mocks the behavior it would have on a
/// real homeserver.
///
/// Inserts all the `DeviceKeys` into `Keys::device_keys`, or if already present
/// in this mapping, only merge the signatures.
fn mock_keys_upload(
    keys: Arc<Mutex<Keys>>,
    token_to_user_id_map: Arc<Mutex<BTreeMap<String, OwnedUserId>>>,
) -> impl Fn(&Request) -> ResponseTemplate {
    move |req: &Request| {
        #[derive(Debug, serde::Deserialize)]
        struct Parameters {
            device_keys: Option<Raw<DeviceKeys>>,
            one_time_keys: Option<BTreeMap<OwnedOneTimeKeyId, Raw<OneTimeKey>>>,
        }
        let bearer_token = req
            .headers
            .get(http::header::AUTHORIZATION)
            .and_then(|header| header.to_str().ok())
            .expect("This call should be authenticated");

        let params: Parameters = req.body_json().unwrap();

        let tokens = token_to_user_id_map.lock().unwrap();
        // Get the user
        let user_id = tokens.get(bearer_token)
            .expect("Expect this token to be known, ensure you use `MatrixKeysMockServer::createClient`")
            .to_owned();

        if let Some(new_device_keys) = params.device_keys {
            let new_device_keys = new_device_keys.deserialize().unwrap();

            let key_id = new_device_keys.device_id.to_string();
            // if known_devices.contains(&key_id) {
            let mut keys = keys.lock().unwrap();
            let devices = keys.device.entry(new_device_keys.user_id.clone()).or_default();

            // Either merge signatures if an entry is already present, or insert a new one.
            if let Some(device_keys) = devices.get_mut(&key_id) {
                let mut existing = device_keys.deserialize().unwrap();

                // Merge signatures.
                for (uid, sigs) in existing.signatures.iter_mut() {
                    if let Some(new_sigs) = new_device_keys.signatures.get(uid) {
                        sigs.extend(new_sigs.clone());
                    }
                }
                for (uid, sigs) in new_device_keys.signatures.iter() {
                    if !existing.signatures.contains_key(uid) {
                        existing.signatures.insert(uid.clone(), sigs.clone());
                    }
                }

                *device_keys = Raw::new(&existing).unwrap();
            } else {
                devices.insert(key_id, Raw::new(&new_device_keys).unwrap());
            }
        }

        let mut keys = keys.lock().unwrap();

        if let Some(otks) = params.one_time_keys {
            // We need a trick to find out what userId|device this OTK is for.
            // This is not part of the payload, a real server uses the access token(?)
            // Let's look at the signatures to find out
            for (key_id, raw_otk) in otks {
                let otk = raw_otk.deserialize().unwrap();
                match otk {
                    OneTimeKey::SignedKey(signed_key) => {
                        let device_id = signed_key
                            .signatures
                            .first_key_value()
                            .unwrap()
                            .1
                            .keys()
                            .next()
                            .unwrap()
                            .key_name()
                            .to_owned();

                        keys.one_time_keys
                            .entry(user_id.clone())
                            .or_default()
                            .entry(device_id)
                            .or_default()
                            .insert(key_id, raw_otk);
                    }
                    OneTimeKey::Key(_) => {
                        // Ignore this old algorithm,
                    }
                    _ => {}
                }
            }
        }

        let otk_count = keys.one_time_keys.get(&user_id).map(|m| m.len()).unwrap_or(0);
        ResponseTemplate::new(200).set_body_json(json!({
            "one_time_key_counts": {
                "signed_curve25519": otk_count,
            }
        }))
    }
}

/// Mocks a `/keys/device_signing/upload` request for bootstrapping
/// cross-signing.
///
/// Assumes (and asserts) all keys are updated at the same time.
///
/// Saves all the different cross-signing keys into their respective fields of
/// `Keys`.
fn mock_keys_device_signing_upload(
    keys: Arc<Mutex<Keys>>,
) -> impl Fn(&Request) -> ResponseTemplate {
    move |req: &Request| {
        // Accept all cross-signing setups by default.
        #[derive(Debug, serde::Deserialize)]
        struct Parameters {
            master_key: Option<Raw<CrossSigningKey>>,
            self_signing_key: Option<Raw<CrossSigningKey>>,
            user_signing_key: Option<Raw<CrossSigningKey>>,
        }

        let params: Parameters = req.body_json().unwrap();
        assert!(params.master_key.is_some());
        assert!(params.self_signing_key.is_some());
        assert!(params.user_signing_key.is_some());

        let mut keys = keys.lock().unwrap();

        if let Some(key) = params.master_key {
            let deserialized = key.deserialize().unwrap();
            let user_id = deserialized.user_id;
            keys.master.insert(user_id, key);
        }

        if let Some(key) = params.self_signing_key {
            let deserialized = key.deserialize().unwrap();
            let user_id = deserialized.user_id;
            keys.self_signing.insert(user_id, key);
        }

        if let Some(key) = params.user_signing_key {
            let deserialized = key.deserialize().unwrap();
            let user_id = deserialized.user_id;
            keys.user_signing.insert(user_id, key);
        }

        ResponseTemplate::new(200).set_body_json(json!({}))
    }
}

/// Mocks a `/keys/signatures/upload` request.
///
/// Supports merging signatures for master keys or devices keys.
fn mock_keys_signature_upload(keys: Arc<Mutex<Keys>>) -> impl Fn(&Request) -> ResponseTemplate {
    move |req: &Request| {
        #[derive(Debug, serde::Deserialize)]
        #[serde(transparent)]
        struct Parameters(BTreeMap<OwnedUserId, SignedKeys>);

        let params: Parameters = req.body_json().unwrap();

        let mut keys = keys.lock().unwrap();

        for (user, signed_keys) in params.0 {
            for (key_id, raw_key) in signed_keys.iter() {
                // Try to find a field in keys.master.
                if let Some(existing_master_key) = keys.master.get_mut(&user) {
                    let mut existing = existing_master_key.deserialize().unwrap();

                    let target = CrossSigningKeyId::from_parts(
                        ruma::SigningKeyAlgorithm::Ed25519,
                        key_id.try_into().unwrap(),
                    );

                    if existing.keys.contains_key(&target) {
                        let param: CrossSigningKey = serde_json::from_str(raw_key.get()).unwrap();

                        for (uid, sigs) in existing.signatures.iter_mut() {
                            if let Some(new_sigs) = param.signatures.get(uid) {
                                sigs.extend(new_sigs.clone());
                            }
                        }
                        for (uid, sigs) in param.signatures.iter() {
                            if !existing.signatures.contains_key(uid) {
                                existing.signatures.insert(uid.clone(), sigs.clone());
                            }
                        }

                        // Update in map.
                        *existing_master_key = Raw::new(&existing).unwrap();
                        continue;
                    }
                }

                // Otherwise, try to find a field in keys.device.
                // Either merge signatures if an entry is already present, or insert a new
                // entry.
                let known_devices = keys.device.entry(user.clone()).or_default();
                let device_keys = known_devices
                    .get_mut(key_id)
                    .expect("trying to add a signature for a missing key");

                let param: DeviceKeys = serde_json::from_str(raw_key.get()).unwrap();

                let mut existing: DeviceKeys = device_keys.deserialize().unwrap();

                for (uid, sigs) in existing.signatures.iter_mut() {
                    if let Some(new_sigs) = param.signatures.get(uid) {
                        sigs.extend(new_sigs.clone());
                    }
                }
                for (uid, sigs) in param.signatures.iter() {
                    if !existing.signatures.contains_key(uid) {
                        existing.signatures.insert(uid.clone(), sigs.clone());
                    }
                }

                *device_keys = Raw::new(&existing).unwrap();
            }
        }

        ResponseTemplate::new(200).set_body_json(json!({
            "failures": {}
        }))
    }
}

fn mock_keys_claimed_request(keys: Arc<Mutex<Keys>>) -> impl Fn(&Request) -> ResponseTemplate {
    move |req: &Request| {
        // Accept all cross-signing setups by default.
        #[derive(Debug, serde::Deserialize)]
        struct Parameters {
            one_time_keys: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, OneTimeKeyAlgorithm>>,
        }

        let params: Parameters = req.body_json().unwrap();

        let mut keys = keys.lock().unwrap();
        let known_otks = &mut keys.one_time_keys;

        let mut found_one_time_keys: BTreeMap<
            OwnedUserId,
            BTreeMap<OwnedDeviceId, BTreeMap<OwnedOneTimeKeyId, Raw<OneTimeKey>>>,
        > = BTreeMap::new();

        for (user, requested_one_time_keys) in params.one_time_keys {
            for device_id in requested_one_time_keys.keys() {
                let device_id = device_id.clone();
                let found_key = known_otks
                    .entry(user.clone())
                    .or_default()
                    .entry(device_id.clone())
                    .or_default()
                    .pop_first();
                if let Some((id, raw_otk)) = found_key {
                    found_one_time_keys
                        .entry(user.clone())
                        .or_default()
                        .entry(device_id.clone())
                        .or_default()
                        .insert(id, raw_otk.clone());
                }
            }
        }

        ResponseTemplate::new(200).set_body_json(json!({
            "one_time_keys" : found_one_time_keys
        }))
    }
}

/// Mount a Mock on the given server to handle the `GET /sync` endpoint with
/// an optional `since` param that returns a 200 status code with the given
/// response body.
async fn mock_sync_scoped(
    server: &MockServer,
    response_body: impl Serialize,
    since: Option<String>,
) -> MockGuard {
    let mut builder = Mock::given(method("GET")).and(path("/_matrix/client/r0/sync"));

    if let Some(since) = since {
        builder = builder.and(query_param("since", since));
    } else {
        builder = builder.and(query_param_is_missing("since"));
    }

    builder
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .mount_as_scoped(server)
        .await
}
