#![cfg(feature = "experimental-send-custom-to-device")]

use std::{
    collections::{BTreeMap, BTreeSet},
    future,
    sync::Arc,
};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use futures_util::pin_mut;
use matrix_sdk::{assert_next_with_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_base::crypto::CollectStrategy;
use matrix_sdk_common::{
    deserialized_responses::{AlgorithmInfo, EncryptionInfo},
    locks::Mutex,
};
use matrix_sdk_test::{async_test, test_json};
use ruma::{
    device_id,
    events::{AnyToDeviceEvent, ToDeviceEventType},
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
};
use serde_json::json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path_regex},
};

use crate::{recipients_of, record_sent_encrypted_to_device};

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
    .cast_unchecked();

    let sent_messages = record_sent_encrypted_to_device(&matrix_mock_server).await;

    alice
        .encryption()
        .encrypt_and_send_raw_to_device(
            vec![&alice_bob_device],
            "call.keys",
            content_raw,
            CollectStrategy::AllDevices,
        )
        .await
        .unwrap();

    let sent_messages = sent_messages.lock();
    assert_eq!(sent_messages.len(), 1, "a single to-device request should have been sent");

    // The message must have been encrypted for Bob's device, and for nobody else.
    assert_eq!(
        recipients_of(&sent_messages[0]),
        BTreeMap::from([(
            bob_user_id.to_owned(),
            BTreeSet::from([DeviceIdOrAllDevices::DeviceId(bob_device_id.to_owned())]),
        )])
    );
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
    .cast_unchecked();

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
        .encrypt_and_send_raw_to_device(
            vec![&alice_bob_device],
            "call.keys",
            content_raw,
            CollectStrategy::AllDevices,
        )
        .await
        .unwrap();

    assert_eq!(1, result.len());
    let failure = result.first().unwrap();
    assert_eq!(bob_user_id.to_owned(), failure.0);
    assert_eq!(bob_device_id.to_owned(), failure.1);
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
    .cast_unchecked();

    // Capture the event sent by Alice to feed it back to Bob's client later.
    let bob_received_to_device_future =
        server.mock_capture_put_to_device_then_sync_back(alice.user_id().unwrap(), &bob).await;

    alice
        .encryption()
        .encrypt_and_send_raw_to_device(
            vec![&alice_bob_device],
            "call.keys",
            content_raw,
            CollectStrategy::AllDevices,
        )
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

    // wait for event to be fed back to Bob's client
    bob_received_to_device_future.await;

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
    .cast_unchecked();

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
        .encrypt_and_send_raw_to_device(
            vec![&alice_bob_device],
            "call.keys",
            content_raw,
            CollectStrategy::AllDevices,
        )
        .await
        .unwrap();

    assert_eq!(1, result.len());
    let failure = result.first().unwrap();
    assert_eq!(bob_user_id.to_owned(), failure.0);
    assert_eq!(bob_device_id.to_owned(), failure.1);
}

#[async_test]
async fn test_send_encrypted_to_device() {
    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;

    let (alice, bob) = matrix_mock_server.set_up_alice_and_bob_for_encryption().await;
    let bob_user_id = bob.user_id().unwrap();
    let bob_device_id = bob.device_id().unwrap();

    let sent_messages = record_sent_encrypted_to_device(&matrix_mock_server).await;

    let recipients = BTreeMap::from([(
        bob_user_id.to_owned(),
        vec![DeviceIdOrAllDevices::DeviceId(bob_device_id.to_owned())],
    )]);

    let failures = alice
        .send_encrypted_to_device(
            &ToDeviceEventType::from("call.keys"),
            recipients,
            Raw::new(&json!({ "call_id": "" })).unwrap().cast_unchecked(),
        )
        .await
        .unwrap();

    assert!(failures.is_empty(), "no failures expected, got {failures:?}");

    let sent_messages = sent_messages.lock();
    assert_eq!(sent_messages.len(), 1, "a single to-device request should have been sent");

    // The message must have been sent to the requested device, and to nobody else.
    assert_eq!(
        recipients_of(&sent_messages[0]),
        BTreeMap::from([(
            bob_user_id.to_owned(),
            BTreeSet::from([DeviceIdOrAllDevices::DeviceId(bob_device_id.to_owned())]),
        )])
    );
}

#[async_test]
async fn test_send_encrypted_to_device_with_wildcard_expands_to_all_devices() {
    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;

    let (alice, bob) = matrix_mock_server.set_up_alice_and_bob_for_encryption().await;
    let bob_user_id = bob.user_id().unwrap();

    // Give Bob a second device, otherwise there is nothing for the wildcard to
    // expand to.
    let bob_2 = matrix_mock_server
        .set_up_new_device_for_encryption(&bob, device_id!("B0B2B0B2"), vec![&alice])
        .await;

    matrix_mock_server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_change_device(bob_user_id);
        })
        .await;

    let sent_messages = record_sent_encrypted_to_device(&matrix_mock_server).await;

    // `*` must be expanded to all of Bob's known devices.
    let recipients =
        BTreeMap::from([(bob_user_id.to_owned(), vec![DeviceIdOrAllDevices::AllDevices])]);

    let failures = alice
        .send_encrypted_to_device(
            &ToDeviceEventType::from("call.keys"),
            recipients,
            Raw::new(&json!({ "call_id": "" })).unwrap().cast_unchecked(),
        )
        .await
        .unwrap();

    assert!(failures.is_empty(), "no failures expected, got {failures:?}");

    let sent_messages = sent_messages.lock();
    assert_eq!(sent_messages.len(), 1, "a single to-device request should have been sent");

    // The `*` must have been expanded to exactly Bob's two devices, and nothing
    // else should have been sent to.
    assert_eq!(
        recipients_of(&sent_messages[0]),
        BTreeMap::from([(
            bob_user_id.to_owned(),
            BTreeSet::from([
                DeviceIdOrAllDevices::DeviceId(bob.device_id().unwrap().to_owned()),
                DeviceIdOrAllDevices::DeviceId(bob_2.device_id().unwrap().to_owned()),
            ]),
        )])
    );
}

#[async_test]
async fn test_sending_encrypted_to_device_messages_to_an_unknown_device_fails() {
    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;

    let (alice, bob) = matrix_mock_server.set_up_alice_and_bob_for_encryption().await;
    let bob_user_id = bob.user_id().unwrap();

    // Nothing can be encrypted, so nothing should be sent out.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/.*/sendToDevice/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .expect(0)
        .named("send_to_device")
        .mount(matrix_mock_server.server())
        .await;

    let unknown_device_id = device_id!("UNKNOWNDEVICE");
    let recipients = BTreeMap::from([(
        bob_user_id.to_owned(),
        vec![DeviceIdOrAllDevices::DeviceId(unknown_device_id.to_owned())],
    )]);

    let failures = alice
        .send_encrypted_to_device(
            &ToDeviceEventType::from("call.keys"),
            recipients,
            Raw::new(&json!({ "call_id": "" })).unwrap().cast_unchecked(),
        )
        .await
        .unwrap();

    assert_eq!(
        failures,
        BTreeMap::from([(bob_user_id.to_owned(), vec![unknown_device_id.to_owned()])])
    );
}

#[async_test]
async fn test_subscribe_to_encrypted_to_device_messages() {
    let matrix_mock_server = MatrixMockServer::new().await;
    matrix_mock_server.mock_crypto_endpoints_preset().await;

    let (alice, bob) = matrix_mock_server.set_up_alice_and_bob_for_encryption().await;
    let bob_user_id = bob.user_id().unwrap();
    let bob_device_id = bob.device_id().unwrap();

    let messages = alice.subscribe_to_custom_to_device_messages(vec![ToDeviceEventType::from(
        "my.custom.to.device",
    )]);
    pin_mut!(messages);

    let bob_alice_device = bob
        .encryption()
        .get_device(alice.user_id().unwrap(), alice.device_id().unwrap())
        .await
        .unwrap()
        .unwrap();

    let event_synced_future =
        matrix_mock_server.mock_capture_put_to_device_then_sync_back(bob_user_id, &alice).await;

    bob.encryption()
        .encrypt_and_send_raw_to_device(
            vec![&bob_alice_device],
            "my.custom.to.device",
            Raw::new(&json!({ "call_id": "" })).unwrap().cast_unchecked(),
            CollectStrategy::AllDevices,
        )
        .await
        .unwrap();

    event_synced_future.await;

    let message = assert_next_with_timeout!(messages);

    // The message was decrypted, so the subscriber sees the plaintext type, not
    // `m.room.encrypted`.
    assert_eq!(message.raw.get_field::<String>("type").unwrap().unwrap(), "my.custom.to.device");

    // And it carries the encryption info of the sending device.
    assert_let!(Some(encryption_info) = message.encryption_info);
    assert_eq!(encryption_info.sender, bob_user_id);
    assert_eq!(encryption_info.sender_device.as_deref(), Some(bob_device_id));
}
