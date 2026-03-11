// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use matrix_sdk_base::crypto::store::types::DehydratedDeviceKey;
use matrix_sdk_test::async_test;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, path_regex},
};

use crate::logged_in_client_with_server;

async fn mock_encryption_setup(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "one_time_key_counts": { "signed_curve25519": 50 }
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/signatures/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"failures": {}})))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"/_matrix/client/r0/user/.* /account_data/m.secret_storage.default_key"))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

#[async_test]
async fn test_upload_dehydrated_device() {
    let (client, server) = logged_in_client_with_server().await;
    mock_encryption_setup(&server).await;

    client
        .encryption()
        .bootstrap_cross_signing(None)
        .await
        .expect("Failed to bootstrap cross-signing");

    Mock::given(method("PUT"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_id": "VIRTUAL_DEVICE"
        })))
        .mount(&server)
        .await;

    let pickle_key = DehydratedDeviceKey::from_slice(&[1u8; 32]).unwrap();

    client
        .encryption()
        .dehydrated_devices()
        .upload(Some("My Dehydrated Device"), &pickle_key)
        .await
        .expect("Failed to upload dehydrated device");
}

#[async_test]
async fn test_rehydrate_returns_ok_on_404() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "No dehydrated device found"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let pickle_key = DehydratedDeviceKey::from_slice(&[1u8; 32]).unwrap();

    client
        .encryption()
        .dehydrated_devices()
        .rehydrate_and_absorb(&pickle_key)
        .await
        .expect("Expected Ok(()) when server returns 404 Not Found");
}

#[async_test]
async fn test_rehydrate_and_absorb() {
    let (client, server) = logged_in_client_with_server().await;
    mock_encryption_setup(&server).await;

    client
        .encryption()
        .bootstrap_cross_signing(None)
        .await
        .expect("Failed to bootstrap cross-signing");

    let pickle_key = DehydratedDeviceKey::from_slice(&[1u8; 32]).unwrap();

    let (device_id, device_data) = client
        .encryption()
        .dehydrated_devices()
        .generate_mock_data("Mock Device", &pickle_key)
        .await
        .expect("Failed to generate mock device data");

    Mock::given(method("GET"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_id": device_id,
            "device_data": device_data,
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(
            r"/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device/.*/events",
        ))
        .and(wiremock::matchers::body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "events": []
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_id": device_id
        })))
        .expect(1)
        .mount(&server)
        .await;

    client
        .encryption()
        .dehydrated_devices()
        .rehydrate_and_absorb(&pickle_key)
        .await
        .expect("Happy path rehydrate_and_absorb failed");
}

#[async_test]
async fn test_rehydrate_and_absorb_with_pagination() {
    let (client, server) = logged_in_client_with_server().await;
    mock_encryption_setup(&server).await;

    client
        .encryption()
        .bootstrap_cross_signing(None)
        .await
        .expect("Failed to bootstrap cross-signing");

    let pickle_key = DehydratedDeviceKey::from_slice(&[1u8; 32]).unwrap();

    let (device_id, device_data) = client
        .encryption()
        .dehydrated_devices()
        .generate_mock_data("Mock Device", &pickle_key)
        .await
        .expect("Failed to generate mock device data");

    Mock::given(method("GET"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_id": device_id,
            "device_data": device_data,
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(
            r"/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device/.*/events",
        ))
        .and(wiremock::matchers::body_json(json!({})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "events": [{
                "type": "m.room.encryption",
                "sender": "@alice:example.org",
                "content": {},
                "event_id": "$event1",
                "origin_server_ts": 123456
            }],
            "next_batch": "batch_token_2"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(
            r"/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device/.*/events",
        ))
        .and(wiremock::matchers::body_json(json!({ "next_batch": "batch_token_2" })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "events": [],
            "next_batch": null
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "device_id": device_id })))
        .expect(1)
        .mount(&server)
        .await;

    client
        .encryption()
        .dehydrated_devices()
        .rehydrate_and_absorb(&pickle_key)
        .await
        .expect("Rehydrate with pagination failed");
}

#[async_test]
async fn test_rehydrate_infinite_pagination_guard() {
    let (client, server) = logged_in_client_with_server().await;
    mock_encryption_setup(&server).await;

    client.encryption().bootstrap_cross_signing(None).await.unwrap();
    let pickle_key = DehydratedDeviceKey::from_slice(&[1u8; 32]).unwrap();

    let (device_id, device_data) = client
        .encryption()
        .dehydrated_devices()
        .generate_mock_data("Mock Device", &pickle_key)
        .await
        .expect("Failed to generate mock device data");

    Mock::given(method("GET"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_id": device_id,
            "device_data": device_data,
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path_regex(
            r"/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device/.*/events",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "events": [{
                "type": "m.room.encryption",
                "sender": "@alice:example.org",
                "content": {},
                "event_id": "$event1",
                "origin_server_ts": 123456
            }],
            "next_batch": "stuck_token"
        })))
        .expect(1..=2)
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "device_id": device_id })))
        .expect(1)
        .mount(&server)
        .await;

    client
        .encryption()
        .dehydrated_devices()
        .rehydrate_and_absorb(&pickle_key)
        .await
        .expect("Rehydrate infinite pagination guard failed");
}

#[async_test]
async fn test_rehydrate_fails_with_wrong_key() {
    let (client, server) = logged_in_client_with_server().await;
    mock_encryption_setup(&server).await;

    client.encryption().bootstrap_cross_signing(None).await.unwrap();
    let correct_key = DehydratedDeviceKey::from_slice(&[1u8; 32]).unwrap();
    let wrong_key = DehydratedDeviceKey::from_slice(&[2u8; 32]).unwrap();

    let (device_id, device_data) = client
        .encryption()
        .dehydrated_devices()
        .generate_mock_data("Mock Device", &correct_key)
        .await
        .expect("Failed to generate mock device data");

    Mock::given(method("GET"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_id": device_id,
            "device_data": device_data,
        })))
        .mount(&server)
        .await;

    let result = client.encryption().dehydrated_devices().rehydrate_and_absorb(&wrong_key).await;

    assert!(result.is_err(), "Expected rehydration to fail with incorrect pickle key");
}

#[async_test]
async fn test_upload_dehydrated_device_default_name() {
    let (client, server) = logged_in_client_with_server().await;
    mock_encryption_setup(&server).await;

    client.encryption().bootstrap_cross_signing(None).await.unwrap();

    Mock::given(method("PUT"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .and(wiremock::matchers::body_partial_json(json!({
            "initial_device_display_name": "Dehydrated device"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_id": "VIRTUAL_DEVICE"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let pickle_key = DehydratedDeviceKey::from_slice(&[1u8; 32]).unwrap();

    client
        .encryption()
        .dehydrated_devices()
        .upload(None, &pickle_key)
        .await
        .expect("Failed to upload dehydrated device with default name");
}
