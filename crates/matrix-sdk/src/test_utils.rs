//! Testing utilities - DO NOT USE IN PRODUCTION.

#![allow(dead_code)]

use assert_matches2::assert_let;
use matrix_sdk_base::{deserialized_responses::SyncTimelineEvent, SessionMeta};
use ruma::{
    api::MatrixVersion,
    device_id,
    events::{room::message::MessageType, AnySyncMessageLikeEvent, AnySyncTimelineEvent},
    user_id,
};
use url::Url;

pub mod events;

use crate::{
    config::RequestConfig,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    Client, ClientBuilder,
};

/// Checks that an event is a message-like text event with the given text.
#[track_caller]
pub fn assert_event_matches_msg<E: Clone + Into<SyncTimelineEvent>>(event: &E, expected: &str) {
    let event: SyncTimelineEvent = event.clone().into();
    let event = event.event.deserialize().unwrap();
    assert_let!(
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(message)) = event
    );
    let message = message.as_original().unwrap();
    assert_let!(MessageType::Text(text) = &message.content.msgtype);
    assert_eq!(text.body, expected);
}

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

/// Restore the common (Matrix-auth) user session for a client.
pub async fn set_client_session(client: &Client) {
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
}

/// A [`Client`] using the given `homeserver_url` (or localhost:1234), that will
/// never retry any failed requests, and already logged in with an hardcoded
/// Matrix authentication session (the user id and device id are hardcoded too).
pub async fn logged_in_client(homeserver_url: Option<String>) -> Client {
    let client = no_retry_test_client(homeserver_url).await;
    set_client_session(&client).await;
    client
}

/// Like [`test_client_builder`], but with a mocked server too.
#[cfg(not(target_arch = "wasm32"))]
pub async fn test_client_builder_with_server() -> (ClientBuilder, wiremock::MockServer) {
    let server = wiremock::MockServer::start().await;
    let builder = test_client_builder(Some(server.uri()));
    (builder, server)
}

/// Like [`no_retry_test_client`], but with a mocked server too.
#[cfg(not(target_arch = "wasm32"))]
pub async fn no_retry_test_client_with_server() -> (Client, wiremock::MockServer) {
    let server = wiremock::MockServer::start().await;
    let client = no_retry_test_client(Some(server.uri().to_string())).await;
    (client, server)
}

/// Like [`logged_in_client`], but with a mocked server too.
#[cfg(not(target_arch = "wasm32"))]
pub async fn logged_in_client_with_server() -> (Client, wiremock::MockServer) {
    let server = wiremock::MockServer::start().await;
    let client = logged_in_client(Some(server.uri().to_string())).await;
    (client, server)
}

/// Asserts the next item in a [`Stream`] can be loaded in the given timeout in
/// the given timeout in milliseconds.
#[macro_export]
macro_rules! assert_next_with_timeout {
    ($stream:expr, $timeout_ms:expr) => {{
        use futures_util::StreamExt as _;
        tokio::time::timeout(std::time::Duration::from_millis($timeout_ms), $stream.next())
            .await
            .expect("Next event timed out")
            .expect("No next event received")
    }};
}

/// Assert the next item in a [`Stream`] matches the provided pattern in the
/// given timeout in milliseconds.
///
/// If no timeout is provided, a default `100ms` value will be used.
#[macro_export]
macro_rules! assert_next_matches_with_timeout {
    ($stream:expr, $pat:pat) => {
        $crate::assert_next_matches_with_timeout!($stream, $pat => {})
    };
    ($stream:expr, $pat:pat => $arm:expr) => {
        $crate::assert_next_matches_with_timeout!($stream, 100, $pat => $arm)
    };
    ($stream:expr, $timeout_ms:expr, $pat:pat => $arm:expr) => {
        match $crate::assert_next_with_timeout!(&mut $stream, $timeout_ms) {
            $pat => $arm,
            val => {
                ::core::panic!(
                    "assertion failed: `{:?}` does not match `{}`",
                    val, ::core::stringify!($pat)
                );
            }
        }
    };
}

/// Like `assert_let`, but with the possibility to add an optional timeout.
///
/// If not provided, a timeout value of 100 milliseconds is used.
#[macro_export]
macro_rules! assert_let_timeout {
    ($timeout:expr, $pat:pat = $future:expr) => {
        assert_matches2::assert_let!(Ok($pat) = tokio::time::timeout($timeout, $future).await);
    };

    ($pat:pat = $future:expr) => {
        assert_let_timeout!(std::time::Duration::from_millis(100), $pat = $future);
    };
}
