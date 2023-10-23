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
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::async_test;
use ruma::{
    api::{client::keys::upload_signatures::v3::SignedKeys, MatrixVersion},
    encryption::CrossSigningKey,
    owned_device_id, owned_user_id,
    serde::Raw,
    DeviceId, OwnedDeviceId, OwnedUserId,
};
use serde_json::{json, value::RawValue};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

type DeviceKeys = Arc<Mutex<BTreeMap<OwnedUserId, BTreeMap<String, Box<RawValue>>>>>;

struct MockedServer {
    server: MockServer,
    known_devices: Arc<Mutex<HashSet<String>>>,
    #[allow(unused)]
    device_keys: DeviceKeys,
}

impl MockedServer {
    async fn new() -> Self {
        let server = MockServer::start().await;

        let known_devices: Arc<Mutex<HashSet<String>>> = Default::default();
        let device_keys: DeviceKeys = Default::default();

        let device_keys_clone = device_keys.clone();
        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/keys/query"))
            .respond_with(move |req: &Request| {
                #[derive(Debug, serde::Deserialize)]
                struct Parameters {
                    device_keys: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
                }

                let params: Parameters = req.body_json().unwrap();

                let mut device_keys = device_keys_clone.lock().unwrap().clone();

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

                ResponseTemplate::new(200).set_body_json(json!({
                    "device_keys": device_keys
                }))
            })
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/keys/upload"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "one_time_key_counts": {}
            })))
            .mount(&server)
            .await;

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

                // We don't do anything particular with the keys, at this point.

                ResponseTemplate::new(200).set_body_json(json!({}))
            })
            .mount(&server)
            .await;

        let known_devices_clone = known_devices.clone();
        let device_keys_clone = device_keys.clone();
        Mock::given(method("POST"))
            .and(path("/_matrix/client/unstable/keys/signatures/upload"))
            .respond_with(move |req: &Request| {
                #[derive(Debug, serde::Deserialize)]
                #[serde(transparent)]
                struct Parameters(BTreeMap<OwnedUserId, SignedKeys>);

                let params: Parameters = req.body_json().unwrap();

                let mut device_keys = device_keys_clone.lock().unwrap();
                let known_devices = known_devices_clone.lock().unwrap();
                for (user_id, signed_keys) in params.0 {
                    for (key_id, signature) in signed_keys.iter() {
                        if known_devices.contains(key_id) {
                            device_keys
                                .entry(user_id.clone())
                                .or_default()
                                .insert(key_id.to_owned(), signature.to_owned());
                        }
                    }
                }

                ResponseTemplate::new(200).set_body_json(json!({
                    "failures": {}
                }))
            })
            .mount(&server)
            .await;

        Self { server, known_devices, device_keys }
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
