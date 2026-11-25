//! Tests for the MSC4140 delayed-event API: `send_raw(..).into_delayed(..)`,
//! `send_state_event_raw(..).into_delayed(..)` and
//! `Client::update_delayed_event`.

use assert_matches::assert_matches;
use matrix_sdk::{
    config::RequestConfig,
    ruma::api::{client::delayed_events::update_delayed_event::UpdateAction, error::ErrorKind},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::async_test;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_test::{JoinedRoomBuilder, TestDevice, User, event_factory::EventFactory};
#[cfg(feature = "e2e-encryption")]
use ruma::RoomVersionId;
use ruma::time::Duration;
use serde_json::json;
use tokio::time::timeout;
#[cfg(feature = "e2e-encryption")]
use wiremock::matchers::body_partial_json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_json, header_exists, method, path_regex},
};

/// The path of the dedicated MSC4140 endpoint, for any room and transaction ID.
fn delayed_event_path(event_type: &str) -> String {
    format!(r"^/_matrix/client/unstable/org.matrix.msc4140/rooms/.*/delayed_event/{event_type}/.*$")
}

#[async_test]
async fn test_send_raw_into_delayed() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_default_joined_room(&client).await;

    Mock::given(method("PUT"))
        .and(path_regex(delayed_event_path("m.room.message")))
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
        .into_delayed(Duration::from_millis(1000))
        .await
        .unwrap();

    assert_eq!(response.delay_id, "1234");
}

/// When the user has reached the homeserver's maximum number of scheduled
/// delayed events, the homeserver answers with `M_LIMIT_EXCEEDED` and a
/// `Retry-After` set to the send time of the user's next delayed event, which
/// can be far away. With retries disabled for this request (even though the
/// client retries by default), the error is returned right away so the caller
/// can decide what to do.
#[async_test]
async fn test_send_raw_into_delayed_max_scheduled_without_retry() {
    let server = MatrixMockServer::new().await;
    // The mock client disables retries by default; re-enable them so that the
    // per-request override is what actually prevents the retry.
    let client = server
        .client_builder()
        .on_builder(|builder| builder.request_config(RequestConfig::short_retry()))
        .build()
        .await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_default_joined_room(&client).await;

    Mock::given(method("PUT"))
        .and(path_regex(delayed_event_path("m.room.message")))
        .respond_with(
            ResponseTemplate::new(429).insert_header("Retry-After", "1200").set_body_json(json!({
                "errcode": "M_LIMIT_EXCEEDED",
                "error": "The maximum number of delayed events has been reached.",
            })),
        )
        .expect(1)
        .mount(server.server())
        .await;

    let send = room
        .send_raw("m.room.message", json!({ "msgtype": "m.text", "body": "hello" }))
        .with_request_config(RequestConfig::new().disable_retry())
        .into_delayed(Duration::from_millis(1000));

    let error = timeout(Duration::from_secs(5), send.into_future())
        .await
        .expect("the request config override was ignored and the request was retried")
        .unwrap_err();

    assert_matches!(error.client_api_error_kind(), Some(ErrorKind::LimitExceeded(_)));
}

/// A delayed message-like event can be sticky ([MSC4354]): the sticky duration
/// is forwarded to the delayed-event endpoint.
///
/// [MSC4354]: https://github.com/matrix-org/matrix-spec-proposals/pull/4354
#[cfg(feature = "unstable-msc4354")]
#[async_test]
async fn test_send_raw_into_delayed_and_sticky_duration() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_default_joined_room(&client).await;

    Mock::given(method("PUT"))
        .and(path_regex(delayed_event_path("org.matrix.rtc.member")))
        .and(body_json(json!({
            "delay_ms": 1000,
            "content": { "msc4354_sticky_key": "abc" },
            "org.matrix.msc4354.sticky_duration_ms": 600_000,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "delay_id": "1234" })))
        .expect(1)
        .mount(server.server())
        .await;

    let response = room
        .send_raw("org.matrix.rtc.member", json!({ "msc4354_sticky_key": "abc" }))
        .with_sticky_duration(Duration::from_secs(600))
        .into_delayed(Duration::from_millis(1000))
        .await
        .unwrap();
    assert_eq!(response.delay_id, "1234");
}

/// A delayed event goes through the same preparation as an immediate one, so
/// it must be encrypted when the room is encrypted rather than handed to the
/// homeserver in the clear.
#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_send_raw_into_delayed_in_encrypted_room() {
    let alice_user_id = User::Alice;
    let alice_device_id = TestDevice::Default;

    let server = MatrixMockServer::new().await;
    server.mock_crypto_endpoints_preset().await;
    server.mock_room_state_encryption().encrypted().mount().await;

    let alice = server
        .client_builder_for_crypto_end_to_end(alice_user_id.into(), alice_device_id.into())
        .build()
        .await;

    let f = EventFactory::new().sender(alice_user_id).default_room();

    let room = server
        .sync_room(
            &alice,
            JoinedRoomBuilder::default()
                .add_state_event(f.create(alice_user_id, RoomVersionId::V1))
                .add_state_event(f.room_encryption()),
        )
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
                "device_id": alice_device_id,
            },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "delay_id": "1234" })))
        .expect(1)
        .mount(server.server())
        .await;

    let response = room
        .send_raw("m.room.message", json!({ "msgtype": "m.text", "body": "hello" }))
        .into_delayed(Duration::from_millis(1000))
        .await
        .unwrap();

    assert_eq!(response.delay_id, "1234");
}

#[async_test]
async fn test_send_state_event_raw_into_delayed() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;
    let room = server.sync_default_joined_room(&client).await;

    Mock::given(method("PUT"))
        .and(path_regex(delayed_event_path("m.room.topic")))
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
        .send_state_event_raw("m.room.topic", "", json!({ "topic": "hello" }))
        .into_delayed(Duration::from_millis(1000))
        .await
        .unwrap();

    assert_eq!(response.delay_id, "1234");
}

#[async_test]
async fn test_update_delayed_event() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

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

        client.update_delayed_event("1234".to_owned(), action, None).await.unwrap();
    }
}
