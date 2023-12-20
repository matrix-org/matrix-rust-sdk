// The http mocking library is not supported for wasm32
#![cfg(not(target_arch = "wasm32"))]

use matrix_sdk::{
    config::{RequestConfig, SyncSettings},
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    Client, ClientBuilder,
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::test_json;
use ruma::{api::MatrixVersion, device_id, user_id};
use serde::Serialize;
use wiremock::{
    matchers::{header, method, path, path_regex, query_param, query_param_is_missing},
    Mock, MockServer, ResponseTemplate,
};

mod client;
#[cfg(feature = "e2e-encryption")]
mod encryption;
mod matrix_auth;
mod refresh_token;
mod room;
#[cfg(feature = "experimental-widgets")]
mod widget;

matrix_sdk_test::init_tracing_for_tests!();

async fn test_client_builder() -> (ClientBuilder, MockServer) {
    let server = MockServer::start().await;
    let builder =
        Client::builder().homeserver_url(server.uri()).server_versions([MatrixVersion::V1_0]);
    (builder, server)
}

async fn no_retry_test_client() -> (Client, MockServer) {
    let (builder, server) = test_client_builder().await;
    let client =
        builder.request_config(RequestConfig::new().disable_retry()).build().await.unwrap();
    (client, server)
}

async fn logged_in_client() -> (Client, MockServer) {
    let session = MatrixSession {
        meta: SessionMeta {
            user_id: user_id!("@example:localhost").to_owned(),
            device_id: device_id!("DEVICEID").to_owned(),
        },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (client, server) = no_retry_test_client().await;
    client.restore_session(session).await.unwrap();

    (client, server)
}

async fn synced_client() -> (Client, MockServer) {
    let (client, server) = logged_in_client().await;
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
