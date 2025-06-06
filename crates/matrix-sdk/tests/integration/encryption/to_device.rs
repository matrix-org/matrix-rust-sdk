#![cfg(feature = "experimental-send-custom-to-device")]

use std::{future, sync::Arc};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_common::{
    deserialized_responses::{AlgorithmInfo, EncryptionInfo},
    locks::Mutex,
};
use matrix_sdk_test::{async_test, test_json};
use ruma::{
    api::client::to_device::send_event_to_device::v3::Messages, events::AnyToDeviceEvent,
    serde::Raw, MilliSecondsSinceUnixEpoch, OwnedUserId,
};
use serde_json::{json, Value};
use wiremock::{
    matchers::{method, path_regex},
    Mock, Request, ResponseTemplate,
};

#[async_test]
async fn test_encrypt_and_send_to_device() {
    // ===========
    // Happy path, will encrypt and send
    // ============

    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;

    let (alice, bob) = matrix_mock_server.set_up_alice_and_bob_for_encryption().await;
    let bob_user_id = bob.user_id().unwrap();
    let bob_device_id = bob.device_id().unwrap();

    // From the point of view of Alice, Bob now has a device.
    let alice_bob_device = alice
        .encryption()
        .get_device(bob_user_id, bob_device_id)
        .await
        .unwrap()
        .expect("alice sees bob's device");

    let content_raw = Raw::new(&json!({
        "keys": [
            {
                "index": 0,
                "key": "rQuVUQs2sHV8Z2rjhmW+aQ=="
            }
        ],
        "device_id": "VYTOIDPHBO",
        "call_id": "",
        "sent_ts": 1000
    }))
    .unwrap()
    .cast();

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/.*/sendToDevice/m.room.encrypted/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        // Should be called once
        .expect(1)
        .named("send_to_device")
        .mount(matrix_mock_server.server())
        .await;

    alice
        .encryption()
        .encrypt_and_send_raw_to_device(vec![&alice_bob_device], "call.keys", content_raw)
        .await
        .unwrap();
}

#[async_test]
async fn test_encrypt_and_send_to_device_report_failures_server() {
    // ===========
    // Error case, when the to-device fails to send
    // ============

    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;

    let (alice, bob) = matrix_mock_server.set_up_alice_and_bob_for_encryption().await;

    let bob_user_id = bob.user_id().unwrap();
    let bob_device_id = bob.device_id().unwrap();

    let content_raw = Raw::new(&json!({
        "keys": [
            {
                "index": 0,
                "key": "rQuVUQs2sHV8Z2rjhmW+aQ=="
            }
        ],
        "device_id": "VYTOIDPHBO",
        "call_id": "",
        "sent_ts": 1000
    }))
    .unwrap()
    .cast();

    // Fail
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/.*/sendToDevice/m.room.encrypted/.*"))
        .respond_with(ResponseTemplate::new(500))
        // There is retries in place, assert it
        .expect(3)
        .named("send_to_device")
        .mount(matrix_mock_server.server())
        .await;

    let alice_bob_device = alice
        .encryption()
        .get_device(bob_user_id, bob_device_id)
        .await
        .unwrap()
        .expect("alice sees bob's device");

    let result = alice
        .encryption()
        .encrypt_and_send_raw_to_device(vec![&alice_bob_device], "call.keys", content_raw)
        .await
        .unwrap();

    assert_eq!(1, result.len());
    let failure = result.first().unwrap();
    assert_eq!(bob_user_id.to_owned(), failure.0);
    assert_eq!(bob_device_id.to_owned(), failure.1);
}

// A simple mock to capture an encrypted to device message via `sendToDevice`.
// Expect the request payload to be for an encrypted event and to only have one
// message.
fn mock_send_encrypted_to_device_responder(
    sender: OwnedUserId,
    to_device: Arc<Mutex<Option<Value>>>,
) -> impl Fn(&Request) -> ResponseTemplate {
    move |req: &Request| {
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

        *to_device.lock() = Some(event);

        ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY)
    }
}

#[async_test]
async fn test_to_device_event_handler_olm_encryption_info() {
    // ===========
    // Happy path, will encrypt and send
    // ============
    let server = MatrixMockServer::new().await;
    server.mock_crypto_endpoints_preset().await;

    let (alice, bob) = server.set_up_alice_and_bob_for_encryption().await;
    let bob_user_id = bob.user_id().unwrap();
    let bob_device_id = bob.device_id().unwrap();

    // From the point of view of Alice, Bob now has a device.
    let alice_bob_device = alice
        .encryption()
        .get_device(bob_user_id, bob_device_id)
        .await
        .unwrap()
        .expect("alice sees bob's device");

    let content_raw = Raw::new(&json!({
        "keys": [
            {
                "index": 0,
                "key": "rQuVUQs2sHV8Z2rjhmW+aQ=="
            }
        ],
        "device_id": "VYTOIDPHBO",
        "call_id": "",
        "sent_ts": 1000
    }))
    .unwrap()
    .cast();

    // Capture the event sent by Alice to feed it back to Bob's client later.
    let event_as_sent_by_alice: Arc<Mutex<Option<Value>>> = Default::default();
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/.*/sendToDevice/m.room.encrypted/.*"))
        .respond_with(mock_send_encrypted_to_device_responder(
            alice.user_id().unwrap().to_owned(),
            event_as_sent_by_alice.clone(),
        ))
        // Should be called once
        .expect(1)
        .named("send_to_device")
        .mount(&server.server())
        .await;

    alice
        .encryption()
        .encrypt_and_send_raw_to_device(vec![&alice_bob_device], "call.keys", content_raw)
        .await
        .unwrap();

    let handled_event_info: Arc<Mutex<(Option<AnyToDeviceEvent>, Option<EncryptionInfo>)>> =
        Default::default();

    bob.add_event_handler({
        let handled_event_info = handled_event_info.clone();
        move |ev: AnyToDeviceEvent, encryption_info: Option<EncryptionInfo>| {
            *handled_event_info.lock() = (Some(ev), encryption_info);
            future::ready(())
        }
    });

    // feed back the event to Bob's client
    let event_as_sent_by_alice = event_as_sent_by_alice.lock().clone().unwrap();
    server
        .mock_sync()
        .ok_and_run(&bob, |builder| {
            builder.add_to_device_event(event_as_sent_by_alice);
        })
        .await;

    let (event, encryption_info) = handled_event_info.lock().clone();
    assert_let!(Some(event) = event);
    assert_eq!(event.event_type().to_string(), "call.keys");
    assert_let!(Some(encryption_info) = encryption_info);
    assert_matches!(encryption_info.algorithm_info, AlgorithmInfo::OlmV1Curve25519AesSha2 { .. });
}

#[async_test]
async fn test_encrypt_and_send_to_device_report_failures_encryption_error() {
    // ===========
    // Error case, when the encryption fails
    // ============

    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;

    let (alice, bob) = matrix_mock_server.set_up_alice_and_bob_for_encryption().await;
    let bob_user_id = bob.user_id().unwrap();
    let bob_device_id = bob.device_id().unwrap();

    let content_raw = Raw::new(&json!({
        "keys": [
            {
                "index": 0,
                "key": "rQuVUQs2sHV8Z2rjhmW+aQ=="
            }
        ],
        "device_id": "VYTOIDPHBO",
        "call_id": "",
        "sent_ts": 1000
    }))
    .unwrap()
    .cast();

    // Should not be called
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/.*/sendToDevice/m.room.encrypted/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        // Should be called once
        .expect(0)
        .named("send_to_device")
        .mount(matrix_mock_server.server())
        .await;

    let alice_bob_device = alice
        .encryption()
        .get_device(bob_user_id, bob_device_id)
        .await
        .unwrap()
        .expect("alice sees bob's device");

    // Simulate exhausting all one-time keys
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/.*/keys/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "one_time_keys": {}
        })))
        // Take priority
        .with_priority(1)
        .mount(matrix_mock_server.server())
        .await;

    let result = alice
        .encryption()
        .encrypt_and_send_raw_to_device(vec![&alice_bob_device], "call.keys", content_raw)
        .await
        .unwrap();

    assert_eq!(1, result.len());
    let failure = result.first().unwrap();
    assert_eq!(bob_user_id.to_owned(), failure.0);
    assert_eq!(bob_device_id.to_owned(), failure.1);
}
