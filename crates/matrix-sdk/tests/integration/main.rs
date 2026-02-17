// The http mocking library is not supported for wasm32
#![cfg(not(target_family = "wasm"))]
use matrix_sdk::test_utils::logged_in_client_with_server;
use serde::Serialize;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param, query_param_is_missing},
};

mod account;
mod client;
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
#[cfg(feature = "experimental-widgets")]
mod widget;

matrix_sdk_test_utils::init_tracing_for_tests!();

/// Mount a Mock on the given server to handle the `GET /sync` endpoint with
/// an optional `since` param that returns a 200 status code with the given
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
