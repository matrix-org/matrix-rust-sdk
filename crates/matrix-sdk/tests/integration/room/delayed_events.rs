//! Tests for the MSC4140 delayed-event API: `send_raw(..).with_delay(..)`,
//! `Room::send_delayed_state_event_raw` and `Room::update_delayed_event`.

use matrix_sdk::{
    ruma::api::client::delayed_events::update_delayed_event::UpdateAction,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::async_test;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_test::{JoinedRoomBuilder, event_factory::EventFactory};
#[cfg(feature = "e2e-encryption")]
use ruma::{RoomVersionId, device_id, user_id};
use ruma::{room_id, time::Duration};
use serde_json::json;
#[cfg(feature = "e2e-encryption")]
use wiremock::matchers::body_partial_json;
#[cfg(feature = "unstable-msc4354")]
use wiremock::matchers::query_param;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_json, header_exists, method, path_regex, query_param_is_missing},
};

/// The path of the dedicated MSC4140 endpoint, for any room and transaction ID.
fn delayed_event_path(event_type: &str) -> String {
    format!(r"^/_matrix/client/unstable/org.matrix.msc4140/rooms/.*/delayed_event/{event_type}/.*$")
}

#[async_test]
async fn test_send_raw_with_delay() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_joined_room(&client, room_id!("!a:b.c")).await;

    Mock::given(method("PUT"))
        .and(path_regex(delayed_event_path("m.room.message")))
        .and(query_param_is_missing("org.matrix.msc4354.sticky_duration_ms"))
        .and(body_json(json!({
            "delay_ms": 1000,
            "content": { "msgtype": "m.text", "body": "hello" },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "delay_id": "1234" })))
        .expect(1)
        .mount(server.server())
        .await;

    let response = room
        .send_raw("m.room.message", json!({ "msgtype": "m.text", "body": "hello" }))
        .with_delay(Duration::from_millis(1000))
        .await
        .unwrap();

    assert_eq!(response.delay_id, "1234");
}

/// A delayed message-like event can be sticky ([MSC4354]): the sticky duration
/// is forwarded to the delayed-event endpoint.
///
/// [MSC4354]: https://github.com/matrix-org/matrix-spec-proposals/pull/4354
#[cfg(feature = "unstable-msc4354")]
#[async_test]
async fn test_send_raw_with_delay_and_sticky_duration() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_joined_room(&client, room_id!("!a:b.c")).await;

    Mock::given(method("PUT"))
        .and(path_regex(delayed_event_path("org.matrix.rtc.member")))
        .and(query_param("org.matrix.msc4354.sticky_duration_ms", "600000"))
        .and(body_json(json!({
            "delay_ms": 1000,
            "content": { "msc4354_sticky_key": "abc" },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "delay_id": "1234" })))
        .expect(1)
        .mount(server.server())
        .await;

    let response = room
        .send_raw("org.matrix.rtc.member", json!({ "msc4354_sticky_key": "abc" }))
        .with_sticky_duration(Duration::from_secs(600))
        .with_delay(Duration::from_millis(1000))
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

    let alice =
        server.client_builder_for_crypto_end_to_end(alice_user_id, alice_device_id).build().await;

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
    Mock::given(method("PUT"))
        .and(path_regex(delayed_event_path("m.room.encrypted")))
        .and(body_partial_json(json!({
            "delay_ms": 1000,
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "device_id": "ALICEDEVICE",
            },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "delay_id": "1234" })))
        .expect(1)
        .mount(server.server())
        .await;

    let room = alice.get_room(room_id).expect("Alice should have access to the room");

    let response = room
        .send_raw("m.room.message", json!({ "msgtype": "m.text", "body": "hello" }))
        .with_delay(Duration::from_millis(1000))
        .await
        .unwrap();

    assert_eq!(response.delay_id, "1234");
}

#[async_test]
async fn test_send_delayed_state_event_raw() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_joined_room(&client, room_id!("!a:b.c")).await;

    Mock::given(method("PUT"))
        .and(path_regex(delayed_event_path("m.room.topic")))
        .and(query_param_is_missing("org.matrix.msc4354.sticky_duration_ms"))
        .and(body_json(json!({
            "delay_ms": 1000,
            "state_key": "",
            "content": { "topic": "hello" },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "delay_id": "1234" })))
        .expect(1)
        .mount(server.server())
        .await;

    let response = room
        .send_delayed_state_event_raw(
            "m.room.topic",
            "",
            json!({ "topic": "hello" }),
            Duration::from_millis(1000),
        )
        .await
        .unwrap();

    assert_eq!(response.delay_id, "1234");
}

#[async_test]
async fn test_update_delayed_event() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_joined_room(&client, room_id!("!a:b.c")).await;

    for (action, serialized) in [
        (UpdateAction::Cancel, "cancel"),
        (UpdateAction::Restart, "restart"),
        (UpdateAction::Send, "send"),
    ] {
        // The homeserver only lets the owner of a delayed event update it, so
        // the access token must be sent even though ruma doesn't require it.
        let _guard = Mock::given(method("POST"))
            .and(path_regex(format!(
                r"^/_matrix/client/unstable/org.matrix.msc4140/delayed_events/1234/{serialized}$"
            )))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount_as_scoped(server.server())
            .await;

        room.update_delayed_event("1234".to_owned(), action).await.unwrap();
    }
}
