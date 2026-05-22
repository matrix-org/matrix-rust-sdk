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

use std::sync::{Arc, Mutex};

use assert_matches2::assert_let;
use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{
    Client, encryption::dehydrated_devices::DehydratedDeviceEvent,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::crypto::store::types::DehydratedDeviceKey;
use matrix_sdk_test::async_test;
use ruma::{OwnedDeviceId, owned_device_id, owned_user_id};
use serde_json::{Value, json};
use wiremock::{
    Request,
    matchers::{body_partial_json, method, path},
};

/// Build a client logged in as Alice with all the standard crypto endpoints
/// preset.
async fn alice_client(server: &MatrixMockServer) -> Client {
    server.mock_crypto_endpoints_preset().await;
    let user_id = owned_user_id!("@alice:example.org");
    let device_id = owned_device_id!("4L1C3");
    server.client_builder_for_crypto_end_to_end(&user_id, &device_id).build().await
}

/// Bootstrap cross-signing on the client; required before `create` can sign
/// the dehydrated-device payload it uploads.
async fn bootstrap_cross_signing(client: &Client) {
    client.encryption().bootstrap_cross_signing(None).await.unwrap();
}

/// Captured payload of a `PUT /dehydrated_device` request.
type CapturedDevice = Arc<Mutex<Option<(OwnedDeviceId, Value)>>>;

/// Mount a `PUT /dehydrated_device` mock that captures the uploaded device
/// data into the returned slot. The slot is later consumed to seed the GET
/// mock during rehydration tests.
async fn capture_uploaded_device(server: &MatrixMockServer) -> CapturedDevice {
    let captured: CapturedDevice = Arc::new(Mutex::new(None));
    let sink = captured.clone();
    server
        .mock_put_dehydrated_device()
        .respond_with(move |req: &Request| {
            #[derive(serde::Deserialize)]
            struct Body {
                device_id: OwnedDeviceId,
                device_data: Value,
            }
            let body: Body = req.body_json().expect("valid PUT body");
            *sink.lock().unwrap() = Some((body.device_id.clone(), body.device_data));
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!({ "device_id": body.device_id }))
        })
        .mount()
        .await;
    captured
}

#[async_test]
async fn test_is_supported_ok() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server
        .mock_get_dehydrated_device()
        .ok(&owned_device_id!("DEHYDRATED"), json!({}))
        .mock_once()
        .mount()
        .await;

    assert!(client.encryption().dehydrated_devices().is_supported().await.unwrap());
}

#[async_test]
async fn test_is_supported_not_found() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server.mock_get_dehydrated_device().not_found().mock_once().mount().await;

    assert!(client.encryption().dehydrated_devices().is_supported().await.unwrap());
}

#[async_test]
async fn test_is_supported_unrecognized() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server.mock_get_dehydrated_device().error_unrecognized().mock_once().mount().await;

    assert!(!client.encryption().dehydrated_devices().is_supported().await.unwrap());
}

#[async_test]
async fn test_is_supported_propagates_other_errors() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server.mock_get_dehydrated_device().error500().mock_once().mount().await;

    client
        .encryption()
        .dehydrated_devices()
        .is_supported()
        .await
        .expect_err("server 500 should propagate");
}

#[async_test]
async fn test_create_with_explicit_display_name() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;
    bootstrap_cross_signing(&client).await;

    server.mock_put_dehydrated_device().ok_echo().mock_once().mount().await;

    let mut events = client.encryption().dehydrated_devices().events();
    let pickle_key = DehydratedDeviceKey::new();
    let device_id = client
        .encryption()
        .dehydrated_devices()
        .create(Some("Bespoke offline catcher"), &pickle_key)
        .await
        .unwrap();

    assert_let!(
        Some(Ok(DehydratedDeviceEvent::Created { device_id: emitted })) = events.next().await
    );
    assert_eq!(emitted, device_id);
    assert_let!(
        Some(Ok(DehydratedDeviceEvent::Uploaded { device_id: uploaded })) = events.next().await
    );
    assert_eq!(uploaded, device_id);
}

#[async_test]
async fn test_create_uses_default_display_name() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;
    bootstrap_cross_signing(&client).await;

    // The PUT mock asserts that the upload carries the default display name.
    wiremock::Mock::given(method("PUT"))
        .and(path("/_matrix/client/unstable/org.matrix.msc3814.v1/dehydrated_device"))
        .and(body_partial_json(json!({
            "initial_device_display_name": "Dehydrated device",
        })))
        .respond_with(|req: &Request| {
            #[derive(serde::Deserialize)]
            struct Body {
                device_id: OwnedDeviceId,
            }
            let body: Body = req.body_json().expect("PUT body deserializes");
            wiremock::ResponseTemplate::new(200)
                .set_body_json(json!({ "device_id": body.device_id }))
        })
        .expect(1)
        .mount(server.server())
        .await;

    let pickle_key = DehydratedDeviceKey::new();
    client.encryption().dehydrated_devices().create(None, &pickle_key).await.unwrap();
}

#[async_test]
async fn test_delete_emits_event_on_success() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server
        .mock_delete_dehydrated_device()
        .ok(&owned_device_id!("DEHYDRATED"))
        .mock_once()
        .mount()
        .await;

    let mut events = client.encryption().dehydrated_devices().events();
    client.encryption().dehydrated_devices().delete().await.unwrap();

    assert_let!(Some(Ok(DehydratedDeviceEvent::Deleted)) = events.next().await);
}

#[async_test]
async fn test_delete_silent_on_not_found() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server.mock_delete_dehydrated_device().not_found().mock_once().mount().await;

    let mut events = client.encryption().dehydrated_devices().events();
    client.encryption().dehydrated_devices().delete().await.unwrap();

    // No event should have fired; the broadcast channel must remain empty.
    assert!(events.next().now_or_never().is_none());
}

#[async_test]
async fn test_delete_silent_on_unrecognized() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server.mock_delete_dehydrated_device().error_unrecognized().mock_once().mount().await;

    let mut events = client.encryption().dehydrated_devices().events();
    client.encryption().dehydrated_devices().delete().await.unwrap();

    assert!(events.next().now_or_never().is_none());
}

#[async_test]
async fn test_rehydrate_round_trip() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;
    bootstrap_cross_signing(&client).await;
    let pickle_key = DehydratedDeviceKey::new();

    // First create a real dehydrated device, capturing its payload so the
    // subsequent GET can hand the same bytes back.
    let captured = capture_uploaded_device(&server).await;
    let device_id =
        client.encryption().dehydrated_devices().create(None, &pickle_key).await.unwrap();

    let (uploaded_id, uploaded_data) =
        captured.lock().unwrap().clone().expect("PUT mock recorded the upload");
    assert_eq!(uploaded_id, device_id);

    server.mock_get_dehydrated_device().ok(&uploaded_id, uploaded_data).mock_once().mount().await;
    server
        .mock_dehydrated_device_events()
        .match_missing_next_batch()
        .ok(vec![], None)
        .mock_once()
        .mount()
        .await;
    server.mock_delete_dehydrated_device().ok(&uploaded_id).mock_once().mount().await;

    let mut events = client.encryption().dehydrated_devices().events();
    let outcome = client.encryption().dehydrated_devices().rehydrate(&pickle_key).await.unwrap();
    assert!(outcome);

    assert_let!(
        Some(Ok(DehydratedDeviceEvent::RehydrationStarted { device_id: started })) =
            events.next().await
    );
    assert_eq!(started, device_id);
    assert_let!(
        Some(Ok(DehydratedDeviceEvent::RehydrationCompleted { device_id: completed, .. })) =
            events.next().await
    );
    assert_eq!(completed, device_id);
    assert_let!(Some(Ok(DehydratedDeviceEvent::Deleted)) = events.next().await);
}

#[async_test]
async fn test_rehydrate_paginated() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;
    bootstrap_cross_signing(&client).await;
    let pickle_key = DehydratedDeviceKey::new();

    let captured = capture_uploaded_device(&server).await;
    client.encryption().dehydrated_devices().create(None, &pickle_key).await.unwrap();
    let (uploaded_id, uploaded_data) =
        captured.lock().unwrap().clone().expect("PUT mock recorded the upload");

    server.mock_get_dehydrated_device().ok(&uploaded_id, uploaded_data).mock_once().mount().await;
    // First call: no next_batch in body, return one event and a cursor.
    server
        .mock_dehydrated_device_events()
        .match_missing_next_batch()
        .ok(
            vec![json!({
                "type": "m.dummy",
                "sender": "@bob:example.org",
                "content": {},
            })],
            Some("next-cursor"),
        )
        .mock_once()
        .mount()
        .await;
    // Second call: body carries the cursor, return an empty batch to terminate.
    server
        .mock_dehydrated_device_events()
        .match_next_batch("next-cursor")
        .ok(vec![], None)
        .mock_once()
        .mount()
        .await;
    server.mock_delete_dehydrated_device().ok(&uploaded_id).mock_once().mount().await;

    let outcome = client.encryption().dehydrated_devices().rehydrate(&pickle_key).await.unwrap();
    assert!(outcome);
}

#[async_test]
async fn test_rehydrate_empty_server() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server.mock_get_dehydrated_device().not_found().mock_once().mount().await;

    let mut events = client.encryption().dehydrated_devices().events();
    let pickle_key = DehydratedDeviceKey::new();
    let outcome = client.encryption().dehydrated_devices().rehydrate(&pickle_key).await.unwrap();
    assert!(!outcome);

    assert!(events.next().now_or_never().is_none());
}

#[async_test]
async fn test_rehydrate_wrong_pickle_key() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;
    bootstrap_cross_signing(&client).await;
    let correct_key = DehydratedDeviceKey::new();
    let wrong_key = DehydratedDeviceKey::new();

    let captured = capture_uploaded_device(&server).await;
    client.encryption().dehydrated_devices().create(None, &correct_key).await.unwrap();
    let (uploaded_id, uploaded_data) =
        captured.lock().unwrap().clone().expect("PUT mock recorded the upload");

    server.mock_get_dehydrated_device().ok(&uploaded_id, uploaded_data).mock_once().mount().await;

    // Direct callers of rehydrate() get the failure as the Err return value;
    // RehydrationError is emitted by start() rather than rehydrate() so a
    // direct call here does not produce that event.
    client
        .encryption()
        .dehydrated_devices()
        .rehydrate(&wrong_key)
        .await
        .expect_err("a mismatched pickle key must fail rehydration");
}

#[ignore = "SSSS round trip is covered by the live integration test"]
#[async_test]
async fn test_pickle_key_round_trip_through_sssss() {
    // The wiremock plumbing for a full SecretStore is out of proportion to
    // what this assertion adds on top of the live test; left as a TODO.
}
