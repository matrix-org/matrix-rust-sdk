//! Tests for [`matrix_sdk::Client::subscribe_to_to_device_messages`].

use futures_util::pin_mut;
use matrix_sdk::{assert_next_with_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::async_test;
use ruma::events::ToDeviceEventType;
use serde_json::json;
use stream_assert::assert_pending;

fn custom(event_type: &str) -> ToDeviceEventType {
    ToDeviceEventType::from(event_type)
}

#[async_test]
async fn test_subscribe_to_to_device_messages() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let stream = client.subscribe_to_to_device_messages(vec![custom("m.custom.wanted")]);
    pin_mut!(stream);

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_to_device_event(json!({
                "sender": "@alice:example.com",
                "type": "m.custom.wanted",
                "content": { "a": "test" },
            }));
        })
        .await;

    let message = assert_next_with_timeout!(stream);
    assert_eq!(message.raw.get_field::<String>("type").unwrap().unwrap(), "m.custom.wanted");
    assert_eq!(message.raw.get_field::<String>("sender").unwrap().unwrap(), "@alice:example.com");
    // It was sent in the clear.
    assert!(message.encryption_info.is_none());

    assert_pending!(stream);
}

#[async_test]
async fn test_subscribe_to_to_device_messages_filters_by_type() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let stream = client.subscribe_to_to_device_messages(vec![custom("m.custom.wanted")]);
    pin_mut!(stream);

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_to_device_event(json!({
                    "sender": "@alice:example.com",
                    "type": "m.custom.unwanted",
                    "content": { "a": "test" },
                }))
                .add_to_device_event(json!({
                    "sender": "@alice:example.com",
                    "type": "m.custom.wanted",
                    "content": { "b": "test" },
                }));
        })
        .await;

    // Only the subscribed type made it through.
    let message = assert_next_with_timeout!(stream);
    assert_eq!(message.raw.get_field::<String>("type").unwrap().unwrap(), "m.custom.wanted");

    assert_pending!(stream);
}

#[async_test]
async fn test_subscribe_to_to_device_messages_empty_filter_yields_every_custom_type() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let stream = client.subscribe_to_to_device_messages(vec![]);
    pin_mut!(stream);

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_to_device_event(json!({
                    "sender": "@alice:example.com",
                    "type": "m.custom.first",
                    "content": {},
                }))
                .add_to_device_event(json!({
                    "sender": "@alice:example.com",
                    "type": "m.custom.second",
                    "content": {},
                }));
        })
        .await;

    let message = assert_next_with_timeout!(stream);
    assert_eq!(message.raw.get_field::<String>("type").unwrap().unwrap(), "m.custom.first");
    let message = assert_next_with_timeout!(stream);
    assert_eq!(message.raw.get_field::<String>("type").unwrap().unwrap(), "m.custom.second");

    assert_pending!(stream);
}

/// The to-device traffic the SDK uses for its own crypto machinery arrives
/// Olm-encrypted and is decrypted by the SDK like any other message; it must
/// still never be handed out, even when asked for by name. The same goes for
/// the messages the SDK could not decrypt, which keep the `m.room.encrypted`
/// type.
#[cfg(feature = "experimental-send-custom-to-device")]
#[async_test]
async fn test_subscribe_to_to_device_messages_never_yields_internal_types() {
    use matrix_sdk_base::crypto::CollectStrategy;
    use ruma::serde::Raw;

    let server = MatrixMockServer::new().await;
    server.mock_crypto_endpoints_preset().await;
    let (alice, bob) = server.set_up_alice_and_bob_for_encryption().await;

    // Ask for everything, including internal types by name.
    let stream = alice.subscribe_to_to_device_messages(vec![
        custom("m.dummy"),
        custom("m.room.encrypted"),
        custom("m.custom.wanted"),
    ]);
    pin_mut!(stream);

    let bob_alice_device = bob
        .encryption()
        .get_device(alice.user_id().unwrap(), alice.device_id().unwrap())
        .await
        .unwrap()
        .unwrap();

    let send_encrypted = async |event_type: &str, content: serde_json::Value| {
        let synced =
            server.mock_capture_put_to_device_then_sync_back(bob.user_id().unwrap(), &alice).await;

        bob.encryption()
            .encrypt_and_send_raw_to_device(
                vec![&bob_alice_device],
                event_type,
                Raw::new(&content).unwrap().cast_unchecked(),
                CollectStrategy::AllDevices,
            )
            .await
            .unwrap();

        synced.await;
    };

    // An `m.dummy` is an internal message that is really exchanged between
    // devices, encrypted: Alice decrypts it, but it isn't handed out.
    send_encrypted("m.dummy", json!({})).await;
    assert_pending!(stream);

    // An encrypted message Alice can't decrypt keeps the `m.room.encrypted`
    // type, and isn't handed out either.
    server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_to_device_event(json!({
                "sender": bob.user_id().unwrap(),
                "type": "m.room.encrypted",
                "content": {
                    "algorithm": "m.olm.v1.curve25519-aes-sha2",
                    "sender_key": "nope",
                    "ciphertext": {},
                },
            }));
        })
        .await;
    assert_pending!(stream);

    // A custom message sent the same way is the only one to come through.
    send_encrypted("m.custom.wanted", json!({ "a": "test" })).await;

    let message = assert_next_with_timeout!(stream);
    assert_eq!(message.raw.get_field::<String>("type").unwrap().unwrap(), "m.custom.wanted");
    assert!(message.encryption_info.is_some());

    assert_pending!(stream);
}

#[async_test]
async fn test_subscribe_to_to_device_messages_stops_on_drop() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let stream = client.subscribe_to_to_device_messages(vec![custom("m.custom.wanted")]);
    drop(stream);

    // Dropping the stream deregisters the event handler; a message received
    // afterwards has nobody to go to, and in particular doesn't pile up anywhere.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_to_device_event(json!({
                "sender": "@alice:example.com",
                "type": "m.custom.wanted",
                "content": {},
            }));
        })
        .await;

    // A new subscription only sees what arrives after it was created.
    let stream = client.subscribe_to_to_device_messages(vec![custom("m.custom.wanted")]);
    pin_mut!(stream);
    assert_pending!(stream);
}
