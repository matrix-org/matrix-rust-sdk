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
    future::Future,
    sync::{Arc, Mutex, atomic::Ordering},
};

use assert_matches2::assert_let;
use matrix_sdk_base::crypto::types::events::room::encrypted::EncryptedToDeviceEvent;
use matrix_sdk_test::test_json;
use ruma::{
    CrossSigningKeyId, DeviceId, MilliSecondsSinceUnixEpoch, OneTimeKeyAlgorithm, OwnedDeviceId,
    OwnedOneTimeKeyId, OwnedUserId, UserId,
    api::client::{
        keys::upload_signatures::v3::SignedKeys, to_device::send_event_to_device::v3::Messages,
    },
    encryption::{CrossSigningKey, DeviceKeys, OneTimeKey},
    events::AnyToDeviceEvent,
    owned_device_id, owned_user_id,
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
};
use serde_json::json;
use tracing::Instrument;
use wiremock::{
    Mock, MockGuard, Request, ResponseTemplate,
    matchers::{method, path_regex},
};

use crate::{
    Client,
    test_utils::{
        client::MockClientBuilder,
        mocks::{Keys, MatrixMockServer},
    },
};

/// Stores pending to-device messages for each user and device.
/// To be used with [`MatrixMockServer::capture_put_to_device_traffic`].
pub type PendingToDeviceMessages =
    BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, Vec<Raw<AnyToDeviceEvent>>>>;

/// Extends the `MatrixMockServer` with useful methods to help mocking
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
/// * Start by creating the mock server like this [`MatrixMockServer::new`].
/// * Then mock the crypto API endpoints
///   [`MatrixMockServer::mock_crypto_endpoints_preset`].
/// * Create your test client using
///   [`MatrixMockServer::client_builder_for_crypto_end_to_end`], this is
///   important as it will set up an access token that will allow to know what
///   client is doing what request.
///
/// The [`MatrixMockServer::set_up_alice_and_bob_for_encryption`] will set up
/// two olm machines aware of each other and ready to communicate.
impl MatrixMockServer {
    /// Creates a new [`MockClientBuilder`] configured to use this server and
    /// suitable for usage of the crypto API end points.
    /// Will create a specific access token and some mapping to the associated
    /// user_id.
    pub fn client_builder_for_crypto_end_to_end(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> MockClientBuilder {
        // Create an access token and store the token to user_id mapping
        let next = self.token_counter.fetch_add(1, Ordering::Relaxed);
        let access_token = format!("TOKEN_{next}");

        {
            let mut mappings = self.token_to_user_id_map.lock().unwrap();
            let auth_string = format!("Bearer {access_token}");
            mappings.insert(auth_string, user_id.to_owned());
        }

        MockClientBuilder::new(Some(&self.server.uri())).logged_in_with_token(
            access_token,
            user_id.to_owned(),
            device_id.to_owned(),
        )
    }

    /// Makes the server forget about all the one-time-keys for that device.
    pub fn exhaust_one_time_keys(&self, user_id: OwnedUserId, device_id: OwnedDeviceId) {
        let mut keys = self.keys.lock().unwrap();
        let known_otks = &mut keys.one_time_keys;
        known_otks.entry(user_id).or_default().entry(device_id).or_default().clear();
    }

    /// Ensure that the given clients are aware of each others public
    /// identities.
    pub async fn exchange_e2ee_identities(&self, alice: &Client, bob: &Client) {
        let alice_user_id = alice.user_id().expect("Alice should have a user ID configured");
        let bob_user_id = bob.user_id().expect("Bob should have a user ID configured");

        let alice_span = tracing::info_span!("alice", user_id=%alice_user_id);
        let bob_span = tracing::info_span!("bob", user_id=%bob_user_id);

        // Have Alice track Bob, so she queries his keys later.
        alice.update_tracked_users_for_testing([bob_user_id]).instrument(alice_span.clone()).await;

        // let bob be aware of Alice keys in order to be able to decrypt custom
        // to-device (the device keys check are deferred for `m.room.key` so this is not
        // needed for sending room messages for example).
        bob.update_tracked_users_for_testing([alice_user_id]).instrument(bob_span.clone()).await;

        // Have Alice and Bob upload their signed device keys.
        self.mock_sync().ok_and_run(alice, |_x| {}).instrument(alice_span.clone()).await;
        self.mock_sync().ok_and_run(bob, |_x| {}).instrument(bob_span).await;

        // Run a sync so we do send outgoing requests, including the /keys/query for
        // getting bob's identity.
        self.mock_sync().ok_and_run(alice, |_x| {}).instrument(alice_span).await;
    }

    /// Utility to properly setup two clients. These two clients will know about
    /// each others (alice will have downloaded bob device keys).
    pub async fn set_up_alice_and_bob_for_encryption(&self) -> (Client, Client) {
        let alice_user_id = owned_user_id!("@alice:example.org");
        let alice_device_id = owned_device_id!("4L1C3");

        let alice = self
            .client_builder_for_crypto_end_to_end(&alice_user_id, &alice_device_id)
            .build()
            .await;

        let bob_user_id = owned_user_id!("@bob:example.org");
        let bob_device_id = owned_device_id!("B0B0B0B0B");
        let bob =
            self.client_builder_for_crypto_end_to_end(&bob_user_id, &bob_device_id).build().await;

        self.exchange_e2ee_identities(&alice, &bob).await;

        (alice, bob)
    }

    /// Creates a third client for e2e tests.
    pub async fn set_up_carl_for_encryption(&self, alice: &Client, bob: &Client) -> Client {
        let carl_user_id = owned_user_id!("@carlg:example.org");
        let carl_device_id = owned_device_id!("CARL_DEVICE");

        let carl =
            self.client_builder_for_crypto_end_to_end(&carl_user_id, &carl_device_id).build().await;

        // Let carl upload it's device keys.
        self.mock_sync().ok_and_run(&carl, |_| {}).await;

        // Have Alice track Carl, so she queries his keys later.
        alice.update_tracked_users_for_testing([carl.user_id().unwrap()]).await;

        // Have Bob track Carl, so she queries his keys later.
        bob.update_tracked_users_for_testing([carl.user_id().unwrap()]).await;

        // Have Alice and Bob upload their signed device keys, and download Carl's keys.
        {
            self.mock_sync().ok_and_run(alice, |_| {}).await;
            self.mock_sync().ok_and_run(bob, |_| {}).await;
        }

        // Let carl be aware of Alice and Bob keys.
        carl.update_tracked_users_for_testing([alice.user_id().unwrap(), bob.user_id().unwrap()])
            .await;

        // A last sync for carl to get the keys.
        self.mock_sync().ok_and_run(alice, |_| {}).await;

        carl
    }

    /// Creates a new device and returns a new client for it.
    /// The new and old clients will be aware of each other.
    ///
    /// # Arguments
    ///
    /// * `existing_client` - The original client for which a new device will be
    ///   created
    /// * `device_id` - The device ID to use for the new client
    /// * `clients_to_update` - A vector of client references that should be
    ///   notified about the new device. These clients will receive a device
    ///   list change notification during their next sync.
    ///
    /// # Returns
    ///
    /// Returns the newly created client instance configured for the new device.
    pub async fn set_up_new_device_for_encryption(
        &self,
        existing_client: &Client,
        device_id: &DeviceId,
        clients_to_update: Vec<&Client>,
    ) -> Client {
        let user_id = existing_client.user_id().unwrap().to_owned();
        let new_device_id = device_id.to_owned();

        let new_client =
            self.client_builder_for_crypto_end_to_end(&user_id, &new_device_id).build().await;

        // sync the keys
        self.mock_sync().ok_and_run(&new_client, |_| {}).await;

        // Notify existing device of a change
        self.mock_sync()
            .ok_and_run(existing_client, |builder| {
                builder.add_change_device(&user_id);
            })
            .await;

        for client_to_update in clients_to_update {
            self.mock_sync()
                .ok_and_run(client_to_update, |builder| {
                    builder.add_change_device(&user_id);
                })
                .await;
        }

        new_client
    }

    /// Mock up the various crypto API so that it can serve back keys when
    /// needed
    pub async fn mock_crypto_endpoints_preset(&self) {
        let keys = &self.keys;
        let token_map = &self.token_to_user_id_map;

        Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/.*/keys/query"))
            .respond_with(mock_keys_query(keys.clone()))
            .mount(&self.server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/.*/keys/upload"))
            .respond_with(mock_keys_upload(keys.clone(), token_map.clone()))
            .mount(&self.server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/.*/keys/device_signing/upload"))
            .respond_with(mock_keys_device_signing_upload(keys.clone()))
            .mount(&self.server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/.*/keys/signatures/upload"))
            .respond_with(mock_keys_signature_upload(keys.clone()))
            .mount(&self.server)
            .await;

        Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/.*/keys/claim"))
            .respond_with(mock_keys_claimed_request(keys.clone()))
            .mount(&self.server)
            .await;
    }

    /// Creates a response handler for mocking encrypted to-device message
    /// requests.
    ///
    /// This function creates a response handler that captures encrypted
    /// to-device messages sent via the `/sendToDevice` endpoint.
    ///
    /// # Arguments
    ///
    /// * `sender` - The user ID of the message sender
    ///
    /// # Returns
    ///
    /// Returns a tuple containing:
    /// - A `MockGuard` the end-point mock is scoped to this guard
    /// - A `Future` that resolves to a `Raw<EncryptedToDeviceEvent>>`
    ///   containing the captured encrypted to-device message.
    ///
    /// # Examples
    ///
    /// ```rust
    /// # use ruma::{ device_id,  user_id, serde::Raw};
    /// # use serde_json::json;
    ///
    /// # use matrix_sdk_test::async_test;
    /// # use matrix_sdk::test_utils::mocks::MatrixMockServer;
    /// #
    /// #[async_test]
    /// async fn test_mock_capture_put_to_device() {
    ///     let server = MatrixMockServer::new().await;
    ///     server.mock_crypto_endpoints_preset().await;
    ///
    ///     let (alice, bob) = server.set_up_alice_and_bob_for_encryption().await;
    ///     let bob_user_id = bob.user_id().unwrap();
    ///     let bob_device_id = bob.device_id().unwrap();
    ///
    ///     // From the point of view of Alice, Bob now has a device.
    ///     let alice_bob_device = alice
    ///         .encryption()
    ///         .get_device(bob_user_id, bob_device_id)
    ///         .await
    ///         .unwrap()
    ///         .expect("alice sees bob's device");
    ///
    ///     let content_raw = Raw::new(&json!({ /*...*/ })).unwrap().cast();
    ///
    ///     // Set up the mock to capture encrypted to-device messages
    ///     let (guard, captured) =
    ///         server.mock_capture_put_to_device(alice.user_id().unwrap()).await;
    ///
    ///     alice
    ///         .encryption()
    ///         .encrypt_and_send_raw_to_device(
    ///             vec![&alice_bob_device],
    ///             "call.keys",
    ///             content_raw,
    ///         )
    ///         .await
    ///         .unwrap();
    ///
    ///     // this is the captured event as sent by alice!
    ///     let sent_event = captured.await;
    ///     drop(guard);
    /// }
    /// ```
    pub async fn mock_capture_put_to_device(
        &self,
        sender_user_id: &UserId,
    ) -> (MockGuard, impl Future<Output = Raw<EncryptedToDeviceEvent>> + use<>) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let tx = Arc::new(Mutex::new(Some(tx)));

        let sender = sender_user_id.to_owned();
        let guard = Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/.*/sendToDevice/m.room.encrypted/.*"))
            .respond_with(move |req: &Request| {
                #[derive(Debug, serde::Deserialize)]
                struct Parameters {
                    messages: Messages,
                }

                let params: Parameters = req.body_json().unwrap();

                let (_, device_to_content) = params.messages.first_key_value().unwrap();
                let content = device_to_content.first_key_value().unwrap().1;

                let event = json!({
                    "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
                    "sender": sender,
                    "type": "m.room.encrypted",
                    "content": content,
                });
                let event: Raw<EncryptedToDeviceEvent> = serde_json::from_value(event).unwrap();

                if let Ok(mut guard) = tx.lock()
                    && let Some(tx) = guard.take()
                {
                    let _ = tx.send(event);
                }

                ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY)
            })
            // Should be called once
            .expect(1)
            .named("send_to_device")
            .mount_as_scoped(self.server())
            .await;

        let future =
            async move { rx.await.expect("Failed to receive captured value - sender was dropped") };

        (guard, future)
    }

    /// Captures a to-device message when it is sent to the mock server and then
    /// injects it into the recipient's sync response.
    ///
    /// This is a utility function that combines capturing an encrypted
    /// to-device message and delivering it to the recipient through a sync
    /// response. It's useful for testing end-to-end encryption scenarios
    /// where you need to verify message delivery and processing.
    ///
    /// # Arguments
    ///
    /// * `sender_user_id` - The user ID of the message sender
    /// * `recipient` - The client that will receive the message through sync
    ///
    /// # Returns
    ///
    /// Returns a `Future` that will resolve when the captured event has been
    /// fed back down the recipient sync.
    pub async fn mock_capture_put_to_device_then_sync_back<'a>(
        &'a self,
        sender_user_id: &UserId,
        recipient: &'a Client,
    ) -> impl Future<Output = Raw<EncryptedToDeviceEvent>> + 'a {
        let (guard, sent_event) = self.mock_capture_put_to_device(sender_user_id).await;

        async {
            let sent_event = sent_event.await;
            drop(guard);
            self.mock_sync()
                .ok_and_run(recipient, |sync_builder| {
                    sync_builder.add_to_device_event(sent_event.deserialize_as().unwrap());
                })
                .await;

            sent_event
        }
    }

    /// Utility to capture all the `/toDevice` upload traffic and store it in
    /// a queue to be later used with
    /// [`MatrixMockServer::sync_back_pending_to_device_messages`].
    pub async fn capture_put_to_device_traffic(
        &self,
        sender_user_id: &UserId,
        to_device_queue: Arc<Mutex<PendingToDeviceMessages>>,
    ) -> MockGuard {
        let sender = sender_user_id.to_owned();

        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/.*/sendToDevice/([^/]+)/.*"))
            .respond_with(move |req: &Request| {
                #[derive(Debug, serde::Deserialize)]
                struct Parameters {
                    messages: Messages,
                }

                let params: Parameters = req.body_json().unwrap();
                let messages = params.messages;

                // Access the captured groups from the path
                let event_type = req
                    .url
                    .path_segments()
                    .and_then(|segments| segments.rev().nth(1))
                    .expect("Event type should be captured in the path");

                let mut to_device_queue = to_device_queue.lock().unwrap();
                for (user_id, device_map) in messages.iter() {
                    for (device_id, content) in device_map.iter() {
                        assert_let!(DeviceIdOrAllDevices::DeviceId(device_id) = device_id);

                        let event = json!({
                            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
                            "sender": sender,
                            "type": event_type.to_owned(),
                            "content": content,
                        });

                        to_device_queue
                            .entry(user_id.to_owned())
                            .or_default()
                            .entry(device_id.to_owned())
                            .or_default()
                            .push(serde_json::from_value(event).unwrap());
                    }
                }

                ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY)
            })
            .mount_as_scoped(self.server())
            .await
    }

    /// Sync the pending to-device messages for this client.
    ///
    /// To be used in connection with
    /// [`MatrixMockServer::capture_put_to_device_traffic`] that is
    /// capturing the traffic.
    pub async fn sync_back_pending_to_device_messages(
        &self,
        to_device_queue: Arc<Mutex<PendingToDeviceMessages>>,
        recipient: &Client,
    ) {
        let messages_to_sync = {
            let to_device_queue = to_device_queue.lock().unwrap();
            let pending_messages = to_device_queue
                .get(&recipient.user_id().unwrap().to_owned())
                .and_then(|treemap| treemap.get(&recipient.device_id().unwrap().to_owned()));

            pending_messages.cloned().unwrap_or_default()
        };

        for message in messages_to_sync {
            self.mock_sync()
                .ok_and_run(recipient, |sync_builder| {
                    sync_builder.add_to_device_event(message.deserialize_as().unwrap());
                })
                .await;
        }
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
            .expect("Expect this token to be known, ensure you use `MatrixKeysServer::client_builder_for_crypto_end_to_end`")
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
