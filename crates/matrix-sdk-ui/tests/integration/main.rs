// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use matrix_sdk::{config::RequestConfig, Client, ClientBuilder, Session};
use matrix_sdk_test::test_json;
use ruma::{api::MatrixVersion, device_id, user_id};
use serde::Serialize;
use wiremock::{
    http::Method,
    matchers::{header, method, path, path_regex, query_param, query_param_is_missing},
    Match, Mock, MockServer, Request, ResponseTemplate,
};

#[cfg(feature = "experimental-notification-api")]
mod notification;
#[cfg(feature = "experimental-room-list")]
mod room_list;
mod timeline;

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

#[derive(Copy, Clone)]
struct SlidingSyncMatcher;

impl Match for SlidingSyncMatcher {
    fn matches(&self, request: &Request) -> bool {
        request.url.path() == "/_matrix/client/unstable/org.matrix.msc3575/sync"
            && request.method == Method::Post
    }
}

#[macro_export]
macro_rules! sync_then_assert_request_and_fake_response {
    (
        [$server:ident, $stream:ident]
        assert request = { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $(,)?
    ) => {
        sync_then_assert_request_and_fake_response! {
            [$server, $stream]
            sync matches Some(Ok(_)),
            assert request = { $( $request_json )* },
            respond with = $( ( code $code ) )? { $( $response_json )* },
        }
    };

    (
        [$server:ident, $stream:ident]
        sync matches $sync_result:pat,
        assert request = { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $(,)?
    ) => {
        {
            use wiremock::{Mock, ResponseTemplate, Match as _};
            use assert_matches::assert_matches;
            use serde_json::json;

            let _code = 200;
            $( let _code = $code; )?

            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(ResponseTemplate::new(_code).set_body_json(
                    json!({ $( $response_json )* })
                ))
                .mount_as_scoped(&$server)
                .await;

            let next = $stream.next().await;

            assert_matches!(next, $sync_result, "sync's result");

            for request in $server.received_requests().await.expect("Request recording has been disabled").iter().rev() {
                if SlidingSyncMatcher.matches(request) {
                    let json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

                    if let Err(error) = assert_json_diff::assert_json_matches_no_panic(
                        &json_value,
                        &json!({ $( $request_json )* }),
                        assert_json_diff::Config::new(assert_json_diff::CompareMode::Inclusive),
                    ) {
                        dbg!(json_value);
                        panic!("{}", error);
                    }

                    break;
                }
            }

            next
        }
    };
}
