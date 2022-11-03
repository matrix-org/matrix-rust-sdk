// The http mocking library is not supported for wasm32
#![cfg(not(target_arch = "wasm32"))]

use matrix_sdk::{config::RequestConfig, Client, ClientBuilder, Session};
use ruma::{api::MatrixVersion, device_id, user_id};
use serde::Serialize;
use wiremock::{
    matchers::{header, method, path, query_param, query_param_is_missing},
    Mock, MockServer, ResponseTemplate,
};

mod client;
mod refresh_token;
mod room;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[ctor::ctor]
fn init_logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .init();
}

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
    let session = Session {
        access_token: "1234".to_owned(),
        refresh_token: None,
        user_id: user_id!("@example:localhost").to_owned(),
        device_id: device_id!("DEVICEID").to_owned(),
    };
    let (client, server) = no_retry_test_client().await;
    client.restore_session(session).await.unwrap();

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
