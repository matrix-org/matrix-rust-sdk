// The http mocking library is not supported for wasm32
#![cfg(not(target_arch = "wasm32"))]

use matrix_sdk::{
    config::SyncSettings,
    test_utils::{logged_in_client, no_retry_test_client, test_client_builder},
    Client, ClientBuilder,
};
use matrix_sdk_test::test_json;
use serde::Serialize;
use wiremock::{
    matchers::{header, method, path, path_regex, query_param, query_param_is_missing},
    Mock, MockServer, ResponseTemplate,
};

mod client;
#[cfg(feature = "e2e-encryption")]
mod encryption;
mod matrix_auth;
mod notification;
mod refresh_token;
mod room;
#[cfg(feature = "experimental-widgets")]
mod widget;

matrix_sdk_test::init_tracing_for_tests!();

async fn test_client_builder_with_server() -> (ClientBuilder, MockServer) {
    let server = MockServer::start().await;
    let builder = test_client_builder(Some(server.uri().to_string()));
    (builder, server)
}

async fn no_retry_test_client_with_server() -> (Client, MockServer) {
    let server = MockServer::start().await;
    let client = no_retry_test_client(Some(server.uri().to_string())).await;
    (client, server)
}

async fn logged_in_client_with_server() -> (Client, MockServer) {
    let server = MockServer::start().await;
    let client = logged_in_client(Some(server.uri().to_string())).await;
    (client, server)
}

async fn synced_client() -> (Client, MockServer) {
    let (client, server) = logged_in_client_with_server().await;
    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new();

    let _response = client.sync_once(sync_settings).await.unwrap();

    (client, server)
}

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

/// Mount a Mock on the given server to handle the `GET
/// /rooms/.../state/m.room.encryption` endpoint with an option whether it
/// should return an encryption event or not.
async fn mock_encryption_state(server: &MockServer, is_encrypted: bool) {
    let builder = Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.*room.*encryption.?"))
        .and(header("authorization", "Bearer 1234"));

    if is_encrypted {
        builder
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&*test_json::sync_events::ENCRYPTION_CONTENT),
            )
            .mount(server)
            .await;
    } else {
        builder
            .respond_with(ResponseTemplate::new(404).set_body_json(&*test_json::NOT_FOUND))
            .mount(server)
            .await;
    }
}
