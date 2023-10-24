use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use imbl::HashSet;
use matrix_sdk::{
    config::RequestConfig,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    Client,
};
use matrix_sdk_base::{crypto::EncryptionSyncChanges, SessionMeta};
use matrix_sdk_test::{async_test, SyncResponseBuilder};
use ruma::{
    api::{
        client::{keys::upload_signatures::v3::SignedKeys, sync::sync_events::DeviceLists},
        MatrixVersion,
    },
    assign,
    encryption::{CrossSigningKey, DeviceKeys},
    owned_device_id, owned_user_id,
    serde::Raw,
    DeviceId, DeviceKeyId, OwnedDeviceId, OwnedUserId,
};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

use crate::mock_sync;

#[derive(Debug, Default)]
struct Keys {
    device: BTreeMap<OwnedUserId, BTreeMap<String, Raw<DeviceKeys>>>,
    master: BTreeMap<OwnedUserId, Raw<CrossSigningKey>>,
    self_signing: BTreeMap<OwnedUserId, Raw<CrossSigningKey>>,
    user_signing: BTreeMap<OwnedUserId, Raw<CrossSigningKey>>,
}

struct MockedServer {
    server: MockServer,
    known_devices: Arc<Mutex<HashSet<String>>>,
}

impl MockedServer {
    async fn new() -> Self {
        let server = MockServer::start().await;

        let known_devices: Arc<Mutex<HashSet<String>>> = Default::default();
        let keys = Arc::new(Mutex::new(Keys::default()));

        let k = keys.clone();
        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/keys/query"))
            .respond_with(move |req: &Request| {
                #[derive(Debug, serde::Deserialize)]
                struct Parameters {
                    device_keys: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
                }

                let params: Parameters = req.body_json().unwrap();

                let keys = k.lock().unwrap();
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
            })
            .mount(&server)
            .await;

        let known_devices_clone = known_devices.clone();
        let k = keys.clone();
        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/keys/upload"))
            .respond_with(move |req: &Request| {
                #[derive(Debug, serde::Deserialize)]
                struct Parameters {
                    device_keys: Option<Raw<DeviceKeys>>,
                }

                let params: Parameters = req.body_json().unwrap();

                let mut keys = k.lock().unwrap();
                let known_devices = known_devices_clone.lock().unwrap();
                if let Some(new_device_keys) = params.device_keys {
                    let new_device_keys = new_device_keys.deserialize().unwrap();

                    if known_devices.contains(&new_device_keys.device_id.to_string()) {
                        let devices =
                            keys.device.entry(new_device_keys.user_id.clone()).or_default();

                        let key_id = new_device_keys.device_id.to_string();
                        // Either merge signatures if an entry is already present, or insert a new
                        // one.
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
                }

                ResponseTemplate::new(200).set_body_json(json!({
                    "one_time_key_counts": {}
                }))
            })
            .mount(&server)
            .await;

        let k = keys.clone();
        Mock::given(method("POST"))
            .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
            .respond_with(move |req: &Request| {
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

                let mut keys = k.lock().unwrap();

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
            })
            .mount(&server)
            .await;

        let k = keys.clone();
        Mock::given(method("POST"))
            .and(path("/_matrix/client/unstable/keys/signatures/upload"))
            .respond_with(move |req: &Request| {
                #[derive(Debug, serde::Deserialize)]
                #[serde(transparent)]
                struct Parameters(BTreeMap<OwnedUserId, SignedKeys>);

                let params: Parameters = req.body_json().unwrap();

                let mut keys = k.lock().unwrap();

                for (user, signed_keys) in params.0 {
                    for (key_id, raw_key) in signed_keys.iter() {
                        // Try to find a field in keys.master.
                        if let Some(existing_master_key) = keys.master.get_mut(&user) {
                            let mut existing = existing_master_key.deserialize().unwrap();

                            let target = DeviceKeyId::from_parts(
                                ruma::DeviceKeyAlgorithm::Ed25519,
                                key_id.into(),
                            );

                            if existing.keys.contains_key(&target) {
                                let param: CrossSigningKey =
                                    serde_json::from_str(raw_key.get()).unwrap();

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
                        // Either merge signatures if entry is already present, or insert a new
                        // entry.
                        let known_devices = keys.device.entry(user.clone()).or_default();
                        if let Some(device_keys) = known_devices.get_mut(key_id) {
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
                        } else {
                            known_devices
                                .insert(key_id.to_owned(), Raw::from_json(raw_key.to_owned()));
                        }
                    }
                }

                ResponseTemplate::new(200).set_body_json(json!({
                    "failures": {}
                }))
            })
            .mount(&server)
            .await;

        Self { server, known_devices }
    }

    fn add_known_device(&mut self, device_id: &DeviceId) {
        self.known_devices.lock().unwrap().insert(device_id.to_string());
    }
}

async fn bootstrap_cross_signing(client: &Client) {
    client.encryption().bootstrap_cross_signing(None).await.unwrap();

    let status = client.encryption().cross_signing_status().await.unwrap();
    assert!(status.is_complete());
}

#[async_test]
async fn test_own_verification() {
    let mut server = MockedServer::new().await;

    let user_id = owned_user_id!("@alice:example.org");
    let device_id = owned_device_id!("4L1C3");
    let alice = Client::builder()
        .homeserver_url(server.server.uri())
        .server_versions([MatrixVersion::V1_0])
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap();
    alice
        .restore_session(MatrixSession {
            meta: SessionMeta { user_id: user_id.clone(), device_id: device_id.clone() },
            tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
        })
        .await
        .unwrap();

    server.add_known_device(&device_id);

    // Have Alice bootstrap cross-signing.
    bootstrap_cross_signing(&alice).await;

    // The local device is considered verified by default.
    let own_device = alice.encryption().get_device(&user_id, &device_id).await.unwrap().unwrap();
    assert!(own_device.is_verified());
    assert!(!own_device.is_deleted());

    // Manually re-verifying doesn't change the outcome.
    own_device.verify().await.unwrap();
    assert!(own_device.is_verified());

    // Bootstrapping signed the user identity we created.
    let user_identity = alice.encryption().get_user_identity(&user_id).await.unwrap().unwrap();

    assert_eq!(user_identity.user_id(), user_id);
    assert!(user_identity.is_verified());

    let master_pub_key = user_identity.master_key();
    assert_eq!(master_pub_key.user_id(), user_id);
    assert!(!master_pub_key.keys().is_empty());
    assert_eq!(master_pub_key.keys().iter().count(), 1);

    // Manually re-verifying doesn't change the outcome.
    user_identity.verify().await.unwrap();
    assert!(user_identity.is_verified());
}

#[async_test]
async fn test_unchecked_mutual_verification() {
    let mut server = MockedServer::new().await;

    let alice_user_id = owned_user_id!("@alice:example.org");
    let alice_device_id = owned_device_id!("4L1C3");
    let alice = Client::builder()
        .homeserver_url(server.server.uri())
        .server_versions([MatrixVersion::V1_0])
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap();
    alice
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: alice_user_id.clone(),
                device_id: alice_device_id.clone(),
            },
            tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
        })
        .await
        .unwrap();

    let bob = Client::builder()
        .homeserver_url(server.server.uri())
        .server_versions([MatrixVersion::V1_0])
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap();

    let bob_user_id = owned_user_id!("@bob:example.org");
    let bob_device_id = owned_device_id!("B0B0B0B0B");
    bob.restore_session(MatrixSession {
        meta: SessionMeta { user_id: bob_user_id.clone(), device_id: bob_device_id.clone() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    })
    .await
    .unwrap();

    server.add_known_device(&alice_device_id);
    server.add_known_device(&bob_device_id);

    let alice_verifies_bob =
        alice.encryption().get_verification(bob.user_id().unwrap(), "flow_id").await;
    assert!(alice_verifies_bob.is_none());

    let alice_verifies_bob_request =
        alice.encryption().get_verification_request(&bob_user_id, "flow_id").await;
    assert!(alice_verifies_bob_request.is_none());

    let alice_bob_device =
        alice.encryption().get_device(&bob_user_id, &bob_device_id).await.unwrap();
    assert!(alice_bob_device.is_none());

    // Have both Alice and Bob bootstrap cross-signing.
    bootstrap_cross_signing(&alice).await;
    bootstrap_cross_signing(&bob).await;

    // Have Alice and Bob upload their signed device keys.
    {
        let mut sync_response_builder = SyncResponseBuilder::new();
        mock_sync(&server.server, sync_response_builder.build_json_sync_response(), None).await;
        alice.sync_once(Default::default()).await.unwrap();
        bob.sync_once(Default::default()).await.unwrap();
    }

    // Have Alice track Bob, so she queries his keys later.
    {
        let alice_olm = alice.olm_machine_for_testing().await;
        let alice_olm = alice_olm.as_ref().unwrap();
        alice_olm.update_tracked_users([bob_user_id.as_ref()]).await.unwrap();
    }

    // Run a sync so we do send outgoing requests, including the /keys/query for
    // getting bob's identity.
    let mut sync_response_builder = SyncResponseBuilder::new();
    mock_sync(&server.server, sync_response_builder.build_json_sync_response(), None).await;
    alice.sync_once(Default::default()).await.unwrap();

    // From the point of view of Alice, Bob now has a device.
    let alice_bob_device = alice
        .encryption()
        .get_device(&bob_user_id, &bob_device_id)
        .await
        .unwrap()
        .expect("alice sees bob's device");

    assert!(!alice_bob_device.is_verified());
    assert!(!alice_bob_device.is_deleted());
    assert!(alice_bob_device.verify().await.is_err(), "can't sign the device of another user");

    let alice_bob_ident = alice
        .encryption()
        .get_user_identity(&bob_user_id)
        .await
        .unwrap()
        .expect("alice sees bob's identity");

    alice_bob_ident.verify().await.unwrap();

    // Notify Alice's devices that some identify changed, so it does another
    // /keys/query request.
    {
        let alice_olm = alice.olm_machine_for_testing().await;
        let alice_olm = alice_olm.as_ref().unwrap();
        let changed_devices = &assign!(DeviceLists::default(), {
            changed: vec![bob_user_id.clone()]
        });
        alice_olm
            .receive_sync_changes(EncryptionSyncChanges {
                to_device_events: Default::default(),
                changed_devices,
                one_time_keys_counts: &Default::default(),
                unused_fallback_keys: Default::default(),
                next_batch_token: None,
            })
            .await
            .unwrap();
    }

    mock_sync(&server.server, sync_response_builder.build_json_sync_response(), None).await;
    alice.sync_once(Default::default()).await.unwrap();

    let alice_bob_ident = alice
        .encryption()
        .get_user_identity(&bob_user_id)
        .await
        .unwrap()
        .expect("alice sees bob's identity");
    assert_eq!(alice_bob_ident.user_id(), bob_user_id);
    assert!(alice_bob_ident.is_verified());

    let master_pub_key = alice_bob_ident.master_key();
    assert_eq!(master_pub_key.user_id(), bob_user_id);
    assert!(!master_pub_key.keys().is_empty());
    assert_eq!(master_pub_key.keys().iter().count(), 1);

    let alice_bob_device = alice
        .encryption()
        .get_device(&bob_user_id, &bob_device_id)
        .await
        .unwrap()
        .expect("alice sees bob's device");
    assert!(alice_bob_device.is_verified());
    assert!(alice_bob_device.is_verified_with_cross_signing());
}
