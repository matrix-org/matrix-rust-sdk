//! Tests for the MSC4140 delayed-event API: `send_raw(..).with_delay(..)`,
//! `Room::send_delayed_state_event_raw` and `Room::update_delayed_event`.

use assert_matches2::assert_matches;
use matrix_sdk::{
    Error,
    ruma::{
        api::{
            FeatureFlag,
            client::delayed_events::{DelayParameters, update_delayed_event::UpdateAction},
        },
        events::{MessageLikeEventType, StateEventType},
    },
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{RoomVersionId, device_id, room_id, time::Duration, user_id};
use serde_json::json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_json, method, path_regex},
};

#[async_test]
async fn test_can_homeserver_send_delayed_events() {
    let server = MatrixMockServer::new().await;

    let client = server.client_builder().unstable_features([FeatureFlag::Msc4140]).build().await;
    assert!(client.can_homeserver_send_delayed_events().await.unwrap());

    let client = server.client_builder().build().await;
    assert!(!client.can_homeserver_send_delayed_events().await.unwrap());
}

#[async_test]
async fn test_send_raw_with_delay() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unstable_features([FeatureFlag::Msc4140]).build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_joined_room(&client, room_id!("!a:b.c")).await;

    server
        .mock_room_send()
        .match_delayed_event(Duration::from_millis(1000))
        .for_type(MessageLikeEventType::RoomMessage)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "delay_id": "1234" })))
        .mock_once()
        .mount()
        .await;

    let response = room
        .send_raw("m.room.message", json!({ "msgtype": "m.text", "body": "hello" }))
        .with_delay(DelayParameters::Timeout { timeout: Duration::from_millis(1000) })
        .await
        .unwrap();

    assert_eq!(response.delay_id, "1234");
}

/// A delayed event goes through the same preparation as an immediate one, so
/// it must be encrypted when the room is encrypted rather than handed to the
/// homeserver in the clear.
#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_send_raw_with_delay_in_encrypted_room() {
    let room_id = room_id!("!test:localhost");
    let alice_user_id = user_id!("@alice:localhost");
    let alice_device_id = device_id!("ALICEDEVICE");

    let server = MatrixMockServer::new().await;
    server.mock_crypto_endpoints_preset().await;
    server.mock_room_state_encryption().encrypted().mount().await;

    let alice = server
        .client_builder_for_crypto_end_to_end(alice_user_id, alice_device_id)
        .unstable_features([FeatureFlag::Msc4140])
        .build()
        .await;

    let f = EventFactory::new().sender(alice_user_id).room(room_id);

    server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_state_event(f.create(alice_user_id, RoomVersionId::V1))
                    .add_state_event(f.room_encryption()),
            );
        })
        .await;

    // Sharing the room key requires the member list.
    server
        .mock_get_members()
        .ok(vec![f.member(alice_user_id).into_raw()])
        .mock_once()
        .mount()
        .await;

    // The delayed event must reach the homeserver as `m.room.encrypted`, with
    // the plaintext type and content nowhere in the body.
    server
        .mock_room_send()
        .match_delayed_event(Duration::from_millis(1000))
        .for_type(MessageLikeEventType::RoomEncrypted)
        .body_matches_partial_json(json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "device_id": "ALICEDEVICE",
        }))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "delay_id": "1234" })))
        .mock_once()
        .mount()
        .await;

    let room = alice.get_room(room_id).expect("Alice should have access to the room");

    let response = room
        .send_raw("m.room.message", json!({ "msgtype": "m.text", "body": "hello" }))
        .with_delay(DelayParameters::Timeout { timeout: Duration::from_millis(1000) })
        .await
        .unwrap();

    assert_eq!(response.delay_id, "1234");
}

#[async_test]
async fn test_send_delayed_state_event_raw() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unstable_features([FeatureFlag::Msc4140]).build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_joined_room(&client, room_id!("!a:b.c")).await;

    server
        .mock_room_send_state()
        .match_delayed_event(Duration::from_millis(1000))
        .for_type(StateEventType::RoomTopic)
        .for_key("".to_owned())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "delay_id": "1234" })))
        .mock_once()
        .mount()
        .await;

    let response = room
        .send_delayed_state_event_raw(
            "m.room.topic",
            "",
            json!({ "topic": "hello" }),
            DelayParameters::Timeout { timeout: Duration::from_millis(1000) },
        )
        .await
        .unwrap();

    assert_eq!(response.delay_id, "1234");
}

#[async_test]
async fn test_update_delayed_event() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unstable_features([FeatureFlag::Msc4140]).build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_joined_room(&client, room_id!("!a:b.c")).await;

    for (action, serialized) in [
        (UpdateAction::Cancel, "cancel"),
        (UpdateAction::Restart, "restart"),
        (UpdateAction::Send, "send"),
    ] {
        let _guard = Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/unstable/org.matrix.msc4140/delayed_events/1234$"))
            .and(body_json(json!({ "action": serialized })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount_as_scoped(server.server())
            .await;

        room.update_delayed_event("1234".to_owned(), action).await.unwrap();
    }
}

/// A homeserver without MSC4140 support ignores the unstable delay query
/// parameter and sends the event right away, so the SDK must refuse to
/// schedule anything on such a homeserver rather than let that happen.
#[async_test]
async fn test_delayed_events_require_homeserver_support() {
    let server = MatrixMockServer::new().await;
    // No unstable features advertised.
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_joined_room(&client, room_id!("!a:b.c")).await;

    // Nothing must reach the room endpoints.
    server.mock_room_send().ok(ruma::event_id!("$1")).never().mount().await;
    server.mock_room_send_state().ok(ruma::event_id!("$1")).never().mount().await;
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/unstable/org.matrix.msc4140/delayed_events/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(0)
        .mount(server.server())
        .await;

    let delay = DelayParameters::Timeout { timeout: Duration::from_millis(1000) };

    let error = room
        .send_raw("m.room.message", json!({ "msgtype": "m.text", "body": "hello" }))
        .with_delay(delay.clone())
        .await
        .unwrap_err();
    assert_matches!(error, Error::UnsupportedHomeserverFeature(FeatureFlag::Msc4140));

    let error = room
        .send_delayed_state_event_raw("m.room.topic", "", json!({ "topic": "hello" }), delay)
        .await
        .unwrap_err();
    assert_matches!(error, Error::UnsupportedHomeserverFeature(FeatureFlag::Msc4140));

    let error =
        room.update_delayed_event("1234".to_owned(), UpdateAction::Cancel).await.unwrap_err();
    assert_matches!(error, Error::UnsupportedHomeserverFeature(FeatureFlag::Msc4140));
}
