//! Testing utilities - DO NOT USE IN PRODUCTION.

#![allow(dead_code)]

use matrix_sdk_base::SessionMeta;
use ruma::{api::MatrixVersion, device_id, user_id};
use url::Url;

use crate::{
    config::RequestConfig,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    Client, ClientBuilder,
};

/// A [`ClientBuilder`] fit for testing, using the given `homeserver_url` (or
/// localhost:1234).
pub fn test_client_builder(homeserver_url: Option<String>) -> ClientBuilder {
    let homeserver = homeserver_url
        .map(|url| Url::try_from(url.as_str()).unwrap())
        .unwrap_or_else(|| Url::try_from("http://localhost:1234").unwrap());
    Client::builder().homeserver_url(homeserver).server_versions([MatrixVersion::V1_0])
}

/// A [`Client`] using the given `homeserver_url` (or localhost:1234), that will
/// never retry any failed requests.
pub async fn no_retry_test_client(homeserver_url: Option<String>) -> Client {
    test_client_builder(homeserver_url)
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap()
}

/// A [`Client`] using the given `homeserver_url` (or localhost:1234), that will
/// never retry any failed requests, and already logged in with an hardcoded
/// Matrix authentication session (the user id and device id are hardcoded too).
pub async fn logged_in_client(homeserver_url: Option<String>) -> Client {
    let client = no_retry_test_client(homeserver_url).await;

    client
        .matrix_auth()
        .restore_session(MatrixSession {
            meta: SessionMeta {
                user_id: user_id!("@example:localhost").to_owned(),
                device_id: device_id!("DEVICEID").to_owned(),
            },
            tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
        })
        .await
        .unwrap();

    client
}
