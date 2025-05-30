#![cfg(feature = "experimental-send-custom-to-device")]

use matrix_sdk::test_utils::mocks::encryption::MatrixKeysMockServer;
use matrix_sdk_test::{async_test, test_json};
use ruma::serde::Raw;
use serde_json::json;
use wiremock::{
    matchers::{method, path, path_regex},
    Mock, ResponseTemplate,
};

#[async_test]
async fn test_encrypt_and_send_to_device() {
    // ===========
    // Happy path, will encrypt and send
    // ============

    let matrix_keys_server_mock = MatrixKeysMockServer::new().await;

    let (alice, bob) = matrix_keys_server_mock.set_up_alice_and_bob_for_encryption().await;
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
        .and(path_regex(r"^/_matrix/client/r0/sendToDevice/m.room.encrypted/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        // Should be called once
        .expect(1)
        .named("send_to_device")
        .mount(&matrix_keys_server_mock.server)
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

    let matrix_keys_server_mock = MatrixKeysMockServer::new().await;

    let (alice, bob) = matrix_keys_server_mock.set_up_alice_and_bob_for_encryption().await;

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
        .and(path_regex(r"^/_matrix/client/r0/sendToDevice/m.room.encrypted/.*"))
        .respond_with(ResponseTemplate::new(500))
        // There is retries in place, assert it
        .expect(3)
        .named("send_to_device")
        .mount(&matrix_keys_server_mock.server)
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

#[async_test]
async fn test_encrypt_and_send_to_device_report_failures_encryption_error() {
    // ===========
    // Error case, when the encryption fails
    // ============

    let matrix_keys_server_mock = MatrixKeysMockServer::new().await;

    let (alice, bob) = matrix_keys_server_mock.set_up_alice_and_bob_for_encryption().await;
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
        .and(path_regex(r"^/_matrix/client/r0/sendToDevice/m.room.encrypted/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        // Should be called once
        .expect(0)
        .named("send_to_device")
        .mount(&matrix_keys_server_mock.server)
        .await;

    let alice_bob_device = alice
        .encryption()
        .get_device(bob_user_id, bob_device_id)
        .await
        .unwrap()
        .expect("alice sees bob's device");

    // Simulate exhausting all one-time keys
    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/claim"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "one_time_keys": {}
        })))
        // Take priority
        .with_priority(1)
        .mount(&matrix_keys_server_mock.server)
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
