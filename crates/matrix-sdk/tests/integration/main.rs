#![recursion_limit = "256"]
// The http mocking library is not supported for wasm32
#![cfg(not(target_family = "wasm"))]
#[cfg(feature = "experimental-send-custom-to-device")]
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use matrix_sdk::test_utils::logged_in_client_with_server;
#[cfg(feature = "experimental-send-custom-to-device")]
use matrix_sdk::test_utils::mocks::MatrixMockServer;
#[cfg(feature = "experimental-send-custom-to-device")]
use matrix_sdk_common::locks::Mutex;
#[cfg(feature = "experimental-send-custom-to-device")]
use matrix_sdk_test::test_json;
#[cfg(feature = "experimental-send-custom-to-device")]
use ruma::{
    OwnedUserId, api::client::to_device::send_event_to_device::v3::Messages,
    to_device::DeviceIdOrAllDevices,
};
use serde::Serialize;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param, query_param_is_missing},
};
#[cfg(feature = "experimental-send-custom-to-device")]
use wiremock::{Request, matchers::path_regex};

mod account;
#[cfg(feature = "unstable-msc4426")]
mod automatic_call_status;
mod client;
mod edit_validation;
#[cfg(feature = "e2e-encryption")]
mod encryption;
mod event_cache;
mod latest_event;
mod matrix_auth;
mod media;
mod notification;
mod refresh_token;
mod room;
mod room_preview;
mod send_queue;
mod sync;
mod to_device;
#[cfg(feature = "experimental-widgets")]
mod widget;

matrix_sdk_test_utils::init_tracing_for_tests!();

/// Mount a Mock on the given server to handle the `GET /sync` endpoint with an
/// optional `since` param that returns a 200 status code with the given
/// response body.
async fn mock_sync(server: &MockServer, response_body: impl Serialize, since: Option<String>) {
    let mut builder = Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/sync"))
        .and(header("authorization", "Bearer 1234"));

    if let Some(since) = since {
        builder = builder.and(query_param("since", since));
    } else {
        builder = builder.and(query_param_is_missing("since"));
    }

    builder
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .mount(server)
        .await;
}

/// Mount a `/sendToDevice` mock that records the encrypted to-device messages
/// that are sent out, so that a test can assert on their recipients once the
/// request has gone through.
///
/// Exactly one request is expected to be sent.
#[cfg(feature = "experimental-send-custom-to-device")]
async fn record_sent_encrypted_to_device(
    mock_server: &MatrixMockServer,
) -> Arc<Mutex<Vec<Messages>>> {
    let sent_messages = Arc::new(Mutex::new(Vec::<Messages>::new()));

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/.*/sendToDevice/m\.room\.encrypted/.*"))
        .respond_with({
            let sent_messages = sent_messages.clone();

            move |req: &Request| {
                #[derive(Debug, serde::Deserialize)]
                struct Parameters {
                    messages: Messages,
                }

                let params: Parameters = req.body_json().unwrap();
                sent_messages.lock().push(params.messages);

                ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY)
            }
        })
        .expect(1)
        .named("send_to_device")
        .mount(mock_server.server())
        .await;

    sent_messages
}

/// The recipients of the given to-device messages, as a `user id -> device ids`
/// map.
///
/// The encrypted contents are dropped, we only care about who was sent to.
#[cfg(feature = "experimental-send-custom-to-device")]
fn recipients_of(messages: &Messages) -> BTreeMap<OwnedUserId, BTreeSet<DeviceIdOrAllDevices>> {
    messages
        .iter()
        .map(|(user_id, devices)| (user_id.clone(), devices.keys().cloned().collect()))
        .collect()
}
