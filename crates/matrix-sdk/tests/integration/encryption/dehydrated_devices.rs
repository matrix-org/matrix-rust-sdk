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

use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, Mutex},
};

use assert_matches2::assert_let;
use futures_util::{FutureExt, Stream, StreamExt};
use matrix_sdk::{
    Client,
    encryption::dehydrated_devices::{DehydratedDeviceError, DehydratedDeviceEvent},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::crypto::store::types::DehydratedDeviceKey;
use matrix_sdk_test::async_test;
use ruma::{OwnedDeviceId, owned_device_id, owned_user_id};
use serde_json::{Value, json};
use wiremock::{
    Request,
    matchers::{body_partial_json, method, path, path_regex},
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

/// Mount a stateful account-data mock: PUT bodies are stored per event type
/// and served back by later GETs, with `M_NOT_FOUND` for anything not yet
/// stored. Enough of a backend for the Secret Storage machinery.
async fn mock_stateful_account_data(server: &MatrixMockServer) {
    let store: Arc<Mutex<BTreeMap<String, Value>>> = Arc::new(Mutex::new(BTreeMap::new()));

    let sink = store.clone();
    wiremock::Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/.*/user/[^/]+/account_data/[^/]+$"))
        .respond_with(move |req: &Request| {
            let body: Value = req.body_json().expect("valid account-data PUT body");
            sink.lock().unwrap().insert(account_data_type(req).to_owned(), body);
            wiremock::ResponseTemplate::new(200).set_body_json(json!({}))
        })
        .mount(server.server())
        .await;

    wiremock::Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/.*/user/[^/]+/account_data/[^/]+$"))
        .respond_with(move |req: &Request| {
            match store.lock().unwrap().get(account_data_type(req)) {
                Some(content) => wiremock::ResponseTemplate::new(200).set_body_json(content),
                None => wiremock::ResponseTemplate::new(404).set_body_json(json!({
                    "errcode": "M_NOT_FOUND",
                    "error": "Account data not found",
                })),
            }
        })
        .mount(server.server())
        .await;
}

/// Event type carried in the last path segment of an account-data URL.
fn account_data_type(req: &Request) -> &str {
    req.url.path_segments().and_then(Iterator::last).expect("account-data URL has path segments")
}

/// Drain `events` until the next [`DehydratedDeviceEvent::Uploaded`],
/// returning the uploaded device id.
async fn next_uploaded_device_id<E: Debug>(
    events: &mut (impl Stream<Item = Result<DehydratedDeviceEvent, E>> + Unpin),
) -> OwnedDeviceId {
    loop {
        let event = events.next().await.expect("event stream is open").expect("no skipped events");
        if let DehydratedDeviceEvent::Uploaded { device_id } = event {
            return device_id;
        }
    }
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

    let mut events = client.encryption().dehydrated_devices().state_stream();
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

    let mut events = client.encryption().dehydrated_devices().state_stream();
    client.encryption().dehydrated_devices().delete().await.unwrap();

    assert_let!(Some(Ok(DehydratedDeviceEvent::Deleted)) = events.next().await);
}

#[async_test]
async fn test_delete_silent_on_not_found() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server.mock_delete_dehydrated_device().not_found().mock_once().mount().await;

    let mut events = client.encryption().dehydrated_devices().state_stream();
    client.encryption().dehydrated_devices().delete().await.unwrap();

    // No event should have fired; the broadcast channel must remain empty.
    assert!(events.next().now_or_never().is_none());
}

#[async_test]
async fn test_delete_silent_on_unrecognized() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server.mock_delete_dehydrated_device().error_unrecognized().mock_once().mount().await;

    let mut events = client.encryption().dehydrated_devices().state_stream();
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

    let mut events = client.encryption().dehydrated_devices().state_stream();
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
async fn test_rehydrate_absent_cursor_ends_the_drain() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;
    bootstrap_cross_signing(&client).await;
    let pickle_key = DehydratedDeviceKey::new();

    let captured = capture_uploaded_device(&server).await;
    client.encryption().dehydrated_devices().create(None, &pickle_key).await.unwrap();
    let (uploaded_id, uploaded_data) =
        captured.lock().unwrap().clone().expect("PUT mock recorded the upload");

    server.mock_get_dehydrated_device().ok(&uploaded_id, uploaded_data).mock_once().mount().await;
    // A non-empty batch without a cursor still terminates the drain cleanly.
    server
        .mock_dehydrated_device_events()
        .match_missing_next_batch()
        .ok(
            vec![json!({
                "type": "m.dummy",
                "sender": "@bob:example.org",
                "content": {},
            })],
            None,
        )
        .mock_once()
        .mount()
        .await;
    server.mock_delete_dehydrated_device().ok(&uploaded_id).mock_once().mount().await;

    let outcome = client.encryption().dehydrated_devices().rehydrate(&pickle_key).await.unwrap();
    assert!(outcome);
}

#[async_test]
async fn test_rehydrate_repeated_cursor_keeps_the_device() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;
    bootstrap_cross_signing(&client).await;
    let pickle_key = DehydratedDeviceKey::new();

    let captured = capture_uploaded_device(&server).await;
    client.encryption().dehydrated_devices().create(None, &pickle_key).await.unwrap();
    let (uploaded_id, uploaded_data) =
        captured.lock().unwrap().clone().expect("PUT mock recorded the upload");

    server.mock_get_dehydrated_device().ok(&uploaded_id, uploaded_data).mock_once().mount().await;
    let batch = vec![json!({
        "type": "m.dummy",
        "sender": "@bob:example.org",
        "content": {},
    })];
    server
        .mock_dehydrated_device_events()
        .match_missing_next_batch()
        .ok(batch.clone(), Some("repeated-cursor"))
        .mock_once()
        .mount()
        .await;
    // The server hands back the same cursor again; the drain must stop
    // without deleting the device so a retry can resume the queue.
    server
        .mock_dehydrated_device_events()
        .match_next_batch("repeated-cursor")
        .ok(batch, Some("repeated-cursor"))
        .mock_once()
        .mount()
        .await;
    server.mock_delete_dehydrated_device().ok(&uploaded_id).never().mount().await;

    let mut events = client.encryption().dehydrated_devices().state_stream();
    let error = client
        .encryption()
        .dehydrated_devices()
        .rehydrate(&pickle_key)
        .await
        .expect_err("a truncated drain must surface as an error");
    assert_let!(DehydratedDeviceError::DrainTruncated { to_device_events: 2 } = error);

    assert_let!(Some(Ok(DehydratedDeviceEvent::RehydrationStarted { .. })) = events.next().await);
    assert_let!(Some(Ok(DehydratedDeviceEvent::RehydrationProgress { .. })) = events.next().await);
    assert_let!(Some(Ok(DehydratedDeviceEvent::RehydrationProgress { .. })) = events.next().await);
    // Neither RehydrationCompleted nor Deleted may follow a truncated drain.
    assert!(events.next().now_or_never().is_none());
}

#[async_test]
async fn test_rehydrate_empty_server() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;

    server.mock_get_dehydrated_device().not_found().mock_once().mount().await;

    let mut events = client.encryption().dehydrated_devices().state_stream();
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

#[async_test]
async fn test_start_round_trips_the_pickle_key_through_secret_storage() {
    let server = MatrixMockServer::new().await;
    let client = alice_client(&server).await;
    bootstrap_cross_signing(&client).await;
    mock_stateful_account_data(&server).await;
    let captured = capture_uploaded_device(&server).await;

    let secret_store = client.encryption().secret_storage().create_secret_store().await.unwrap();

    let dehydrated = client.encryption().dehydrated_devices();
    let mut events = dehydrated.state_stream();

    dehydrated.start(&secret_store).await.unwrap();
    let first_id = next_uploaded_device_id(&mut events).await;

    // The generated pickle key must have landed in Secret Storage, not only
    // in the local cache.
    assert!(dehydrated.is_key_stored(&secret_store).await.unwrap());

    // The second start rehydrates the first device and uploads a fresh one.
    let (uploaded_id, uploaded_data) =
        captured.lock().unwrap().clone().expect("PUT mock recorded the upload");
    assert_eq!(uploaded_id, first_id);
    server.mock_get_dehydrated_device().ok(&uploaded_id, uploaded_data).mock_once().mount().await;
    server.mock_dehydrated_device_events().ok(vec![], None).mock_once().mount().await;
    server.mock_delete_dehydrated_device().ok(&uploaded_id).mock_once().mount().await;

    dehydrated.start(&secret_store).await.unwrap();
    let second_id = next_uploaded_device_id(&mut events).await;
    assert_ne!(second_id, first_id, "the second start must upload a fresh dehydrated device");

    dehydrated.stop();
}
