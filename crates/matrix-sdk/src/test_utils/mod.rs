//! Testing utilities - DO NOT USE IN PRODUCTION.

#![allow(dead_code)]

use assert_matches2::assert_let;
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use ruma::{
    api::MatrixVersion,
    events::{room::message::MessageType, AnySyncMessageLikeEvent, AnySyncTimelineEvent},
};
use url::Url;

pub mod client;
#[cfg(not(target_arch = "wasm32"))]
pub mod mocks;

use self::client::mock_matrix_session;
use crate::{config::RequestConfig, Client, ClientBuilder};

/// Checks that an event is a message-like text event with the given text.
#[track_caller]
pub fn assert_event_matches_msg<E: Clone + Into<TimelineEvent>>(event: &E, expected: &str) {
    let event: TimelineEvent = event.clone().into();
    let event = event.raw().deserialize().unwrap();
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
    client.matrix_auth().restore_session(mock_matrix_session()).await.unwrap();
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

/// Asserts that the next item in a `Stream` is received within a given timeout.
///
/// This macro waits for the next item from an asynchronous `Stream` or, if no
/// item is received within the specified timeout, the macro panics.
///
/// # Parameters
///
/// - `$stream`: The `Stream` or `Subscriber` to poll for the next item.
/// - `$timeout_ms` (optional): The timeout in milliseconds to wait for the next
///   item. Defaults to 500ms if not provided.
///
/// # Example
///
/// ```rust
/// use futures_util::{stream, StreamExt};
/// use matrix_sdk::assert_next_with_timeout;
///
/// # async {
/// let mut stream = stream::iter(vec![1, 2, 3]);
/// let next_item = assert_next_with_timeout!(stream, 1000); // Waits up to 1000ms
/// assert_eq!(next_item, 1);
///
/// // The timeout can be omitted, in which case it defaults to 500 ms.
/// let next_item = assert_next_with_timeout!(stream); // Waits up to 500ms
/// assert_eq!(next_item, 2);
/// # };
/// ```
#[macro_export]
macro_rules! assert_next_with_timeout {
    ($stream:expr) => {
        $crate::assert_next_with_timeout!($stream, 500)
    };
    ($stream:expr, $timeout_ms:expr) => {{
        // Needed for subscribers, as they won't use the StreamExt features
        #[allow(unused_imports)]
        use futures_util::StreamExt as _;
        tokio::time::timeout(std::time::Duration::from_millis($timeout_ms), $stream.next())
            .await
            .expect("Next event timed out")
            .expect("No next event received")
    }};
}

/// Asserts the next item in a `Receiver` can be loaded in the given timeout in
/// milliseconds.
#[macro_export]
macro_rules! assert_recv_with_timeout {
    ($receiver:expr, $timeout_ms:expr) => {{
        tokio::time::timeout(std::time::Duration::from_millis($timeout_ms), $receiver.recv())
            .await
            .expect("Next event timed out")
            .expect("No next event received")
    }};
}

/// Assert the next item in a `Stream` or `Subscriber` matches the provided
/// pattern in the given timeout in milliseconds.
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

/// Asserts that the next item from an asynchronous stream is equal to the
/// expected value, with an optional timeout and custom error message.
///
/// # Arguments
///
/// * `$stream` - The asynchronous stream to retrieve the next item from.
/// * `$expected` - The expected value to assert against.
/// * `$timeout ms` (optional) - A timeout in milliseconds (e.g., `200ms`).
///   Defaults to `100ms`.
/// * `$msg` (optional) - A formatted message string for assertion failure.
///
/// # Examples
///
/// ```
/// # async {
/// # use matrix_sdk::assert_next_eq_with_timeout;
/// # use tokio_stream::StreamExt;
///
/// let mut stream = tokio_stream::iter(vec![42]);
/// assert_next_eq_with_timeout!(stream, 42);
///
/// let mut stream = tokio_stream::iter(vec![42]);
/// assert_next_eq_with_timeout!(stream, 42, 200 ms);
///
/// let mut stream = tokio_stream::iter(vec![42]);
/// assert_next_eq_with_timeout!(
///     stream,
///     42,
///     "Expected 42 but got something else"
/// );
///
/// let mut stream = tokio_stream::iter(vec![42]);
/// assert_next_eq_with_timeout!(stream, 42, 200 ms, "Expected 42 within 200ms");
/// # };
/// ```
#[macro_export]
macro_rules! assert_next_eq_with_timeout {
    ($stream:expr, $expected:expr) => {
        $crate::assert_next_eq_with_timeout_impl!($stream, $expected, std::time::Duration::from_millis(100));
    };
    ($stream:expr, $expected:expr, $timeout:literal ms) => {
        $crate::assert_next_eq_with_timeout_impl!($stream, $expected, std::time::Duration::from_millis($timeout));
    };
    ($stream:expr, $expected:expr, $timeout:literal ms, $($msg:tt)*) => {
        $crate::assert_next_eq_with_timeout_impl!($stream, $expected, std::time::Duration::from_millis($timeout), $($msg)*);
    };
    ($stream:expr, $expected:expr, $($msg:tt)*) => {
        $crate::assert_next_eq_with_timeout_impl!($stream, $expected, std::time::Duration::from_millis(100), $($msg)*);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! assert_next_eq_with_timeout_impl {
    ($stream:expr, $expected:expr, $timeout:expr) => {
        let next_value = tokio::time::timeout(
            $timeout,
            $stream.next()
        )
        .await
        .expect("We should be able to get the next value out of the stream by now")
        .expect("The stream should have given us a new value instead of None");

        assert_eq!(next_value, $expected);
    };
    ($stream:expr, $expected:expr, $timeout:expr, $($msg:tt)*) => {{
        let next_value = tokio::time::timeout(
            $timeout,
            futures_util::StreamExt::next(&mut $stream)
        )
        .await
        .expect("We should be able to get the next value out of the stream by now")
        .expect("The stream should have given us a new value instead of None");

        assert_eq!(next_value, $expected, $($msg)*);
    }};
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
