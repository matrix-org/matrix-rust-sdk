// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::{borrow::Cow, time::Duration};

use http::StatusCode;
use matrix_sdk_base::sleep;
use ruma::api::{
    EndpointError, SupportedVersions,
    client::rendezvous::{
        create_rendezvous_session, get_rendezvous_session, update_rendezvous_session,
    },
    error::{FromHttpResponseError, IntoHttpError, MatrixError},
};
use tracing::{debug, instrument, trace};
use url::Url;

use crate::{HttpError, RumaApiError, http_client::HttpClient};

#[cfg(test)]
const POLL_TIMEOUT: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const POLL_TIMEOUT: Duration = Duration::from_secs(1);

/// The result of the [`RendezvousChannel::create_inbound()`] method.
pub(super) struct InboundChannelCreationResult {
    /// The connected [`RendezvousChannel`].
    pub channel: Channel,
    /// The initial message we received when we connected to the
    /// [`RendezvousChannel`].
    ///
    /// This is currently unused, but left in for completeness sake.
    #[allow(dead_code)]
    pub initial_message: String,
}

struct RendezvousMessage {
    pub status_code: StatusCode,
    pub data: String,
}

pub(crate) struct Channel {
    client: HttpClient,
    base_url: Url,
    rendezvous_id: String,
    sequence_token: String,
}

fn response_to_error(status: StatusCode, data: String) -> HttpError {
    match http::Response::builder().status(status).body(data).map_err(IntoHttpError::from) {
        Ok(response) => {
            let error = FromHttpResponseError::<RumaApiError>::Server(RumaApiError::Other(
                MatrixError::from_http_response(response),
            ));

            error.into()
        }
        Err(e) => HttpError::IntoHttp(e),
    }
}

impl Channel {
    /// Create a new outbound [`RendezvousChannel`].
    ///
    /// By outbound we mean that we're going to tell the Matrix server to create
    /// a new rendezvous session. We're going to send an initial empty message
    /// through the channel.
    pub(super) async fn create_outbound(
        client: HttpClient,
        base_url: &Url,
    ) -> Result<Self, HttpError> {
        use std::borrow::Cow;

        let request = create_rendezvous_session::unstable_msc4388::Request::new("".to_owned());

        // TODO: Use the `Client::send()` method here?
        let response = client
            .send(
                request,
                None,
                base_url.to_string(),
                None, // TODO: should also support passing in access token
                Cow::Owned(SupportedVersions {
                    versions: Default::default(),
                    features: Default::default(),
                }),
                Default::default(),
            )
            .await?;

        let rendezvous_id = response.id;
        let sequence_token = response.sequence_token;

        Ok(Self { client, base_url: base_url.to_owned(), rendezvous_id, sequence_token })
    }

    /// Create a new inbound [`RendezvousChannel`].
    ///
    /// By inbound we mean that we're going to attempt to read an initial
    /// message from the rendezvous session on the given [`rendezvous_url`].
    pub(super) async fn create_inbound(
        client: HttpClient,
        base_url: &Url,
        rendezvous_id: &str,
    ) -> Result<InboundChannelCreationResult, HttpError> {
        // Receive the initial message, which should be empty. But we need the ETAG to
        // fully establish the rendezvous channel.
        let response = Self::receive_message_impl(&client, base_url, rendezvous_id).await?;

        let sequence_token = response.sequence_token.clone();

        let initial_message =
            RendezvousMessage { status_code: StatusCode::OK, data: response.data };

        let channel = Self {
            client,
            base_url: base_url.to_owned(),
            rendezvous_id: rendezvous_id.to_owned(),
            sequence_token,
        };

        Ok(InboundChannelCreationResult { channel, initial_message: initial_message.data })
    }

    /// Get the ID of the rendezvous session we're using to exchange messages
    /// through the channel.
    pub(super) fn rendezvous_id(&self) -> &str {
        &self.rendezvous_id
    }

    /// Send the given `message` through the [`RendezvousChannel`] to the other
    /// device.
    #[instrument(skip_all)]
    pub(super) async fn send(&mut self, message: String) -> Result<(), HttpError> {
        let request = update_rendezvous_session::unstable::Request::new(
            self.rendezvous_id.clone(),
            self.sequence_token.clone(),
            message,
        );

        let response = self
            .client
            .send(
                request,
                None,
                self.base_url.to_string(),
                None,
                Cow::Owned(SupportedVersions {
                    versions: Default::default(),
                    features: Default::default(),
                }),
                Default::default(),
            )
            .await?;

        debug!("Response for the rendezvous sending request {response:?}");

        // We successfully send out a message, get the sequence_token and update our
        // internal copy of the sequence_token.
        self.sequence_token = response.sequence_token;

        Ok(())
    }

    /// Attempt to receive a message from the [`RendezvousChannel`] from the
    /// other device.
    ///
    /// The content should be of the `text/plain` content type but the parsing
    /// and verification of this fact is left up to the caller.
    ///
    /// This method will wait in a loop for the channel to give us a new
    /// message.
    pub(super) async fn receive(&mut self) -> Result<String, HttpError> {
        loop {
            let message = self.receive_single_message().await?;

            trace!(
                status_code = %message.status_code,
                "Received data from the rendezvous channel"
            );
            // if sequence token is the same as our current one, it means no new message and
            // map into not modified HTTP status

            if message.status_code == StatusCode::OK {
                return Ok(message.data);
            } else if message.status_code == StatusCode::NOT_MODIFIED {
                sleep::sleep(POLL_TIMEOUT).await;
                continue;
            } else {
                let error = response_to_error(message.status_code, message.data);

                return Err(error);
            }
        }
    }

    async fn receive_message_impl(
        client: &HttpClient,
        base_url: &Url,
        rendezvous_id: &str,
    ) -> Result<get_rendezvous_session::unstable::Response, HttpError> {
        let request = get_rendezvous_session::unstable::Request::new(rendezvous_id.to_owned());
        client
            .send(
                request,
                None,
                base_url.to_string(),
                None,
                Cow::Owned(SupportedVersions {
                    versions: Default::default(),
                    features: Default::default(),
                }),
                Default::default(),
            )
            .await
    }

    async fn receive_single_message(&mut self) -> Result<RendezvousMessage, HttpError> {
        let response =
            Self::receive_message_impl(&self.client, &self.base_url, &self.rendezvous_id).await?;

        // since the rendezvous API has changed it doesn't make sense to use the http
        // status code at this layer
        if response.sequence_token == self.sequence_token {
            return Ok(RendezvousMessage {
                status_code: StatusCode::NOT_MODIFIED,
                data: response.data,
            });
        }

        // We received a new sequence_token, put it into the copy of our sequence_token.
        self.sequence_token = response.sequence_token;

        let message = RendezvousMessage { status_code: StatusCode::OK, data: response.data };

        Ok(message)
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test {
    use matrix_sdk_test::async_test;
    use serde_json::json;
    use similar_asserts::assert_eq;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;
    use crate::config::RequestConfig;

    const BASE_PATH: &str = "/_matrix/client/unstable/io.element.msc4388/rendezvous";

    async fn mock_rendezvous_create(
        server: &MockServer,
        rendezvous_id: &str,
    ) -> Result<String, HttpError> {
        server
            .register(Mock::given(method("POST")).and(path(BASE_PATH)).respond_with(
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": rendezvous_id,
                    "sequence_token": "1",
                    "expires_in_ms": 10_000,
                })),
            ))
            .await;

        Ok(format!("{BASE_PATH}/{rendezvous_id}"))
    }

    #[async_test]
    async fn test_creation() {
        let server = MockServer::start().await;
        let base_url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let rendezvous_id = "abcdEFG12345";

        let rendezvous_path = mock_rendezvous_create(&server, rendezvous_id)
            .await
            .expect("We should be able to create a rendezvous");

        let client = HttpClient::new(reqwest::Client::new(), RequestConfig::new().disable_retry());

        let mut alice = Channel::create_outbound(client, &base_url)
            .await
            .expect("We should be able to create an outbound rendezvous channel");

        assert_eq!(
            alice.rendezvous_id(),
            rendezvous_id,
            "Alice should have configured the rendezvous ID correctly."
        );

        assert_eq!(
            alice.sequence_token, "1",
            "Alice should have remembered the sequence_token the server gave us."
        );

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET")).and(path(rendezvous_path.clone())).respond_with(
                        ResponseTemplate::new(200).set_body_json(json!({
                            "data": "",
                            "sequence_token": "1",
                            "expires_in_ms": 10_000,
                        })),
                    ),
                )
                .await;

            let response = alice
                .receive_single_message()
                .await
                .expect("Alice should be able to wait for data on the rendezvous channel.");
            assert_eq!(response.status_code, StatusCode::NOT_MODIFIED);
        }

        let mut bob = {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET")).and(path(rendezvous_path.clone())).respond_with(
                        ResponseTemplate::new(200).set_body_json(json!({
                            "data": "",
                            "sequence_token": "2",
                            "expires_in_ms": 10_000,
                        })),
                    ),
                )
                .await;

            let client = HttpClient::new(reqwest::Client::new(), RequestConfig::short_retry());
            let InboundChannelCreationResult { channel: bob, initial_message: _ } =
                Channel::create_inbound(client, &base_url, rendezvous_id).await.expect(
                    "We should be able to create a rendezvous channel from a received message",
                );

            assert_eq!(alice.rendezvous_id(), bob.rendezvous_id());

            bob
        };

        assert_eq!(
            bob.sequence_token, "2",
            "Bob should have remembered the sequence_token the server gave us."
        );

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET")).and(path(rendezvous_path.clone())).respond_with(
                        ResponseTemplate::new(200).set_body_json(json!({
                            "data": "",
                            "sequence_token": "2",
                            "expires_in_ms": 10_000,
                        })),
                    ),
                )
                .await;

            let response = alice
                .receive_single_message()
                .await
                .expect("We should be able to wait for data on the rendezvous channel.");
            assert_eq!(response.status_code, StatusCode::OK);
        }

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("PUT")).and(path(rendezvous_path.clone())).respond_with(
                        ResponseTemplate::new(200).set_body_json(json!({
                            "sequence_token": "3",
                            "expires_in_ms": 10_000,
                        })),
                    ),
                )
                .await;

            bob.send("Hello world".to_owned())
                .await
                .expect("We should be able to send data to the rendezvous server.");
        }

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET")).and(path(rendezvous_path.clone())).respond_with(
                        ResponseTemplate::new(200).set_body_json(json!({
                            "data": "Hello world",
                            "sequence_token": "3",
                            "expires_in_ms": 10_000,
                        })),
                    ),
                )
                .await;

            let response = alice
                .receive_single_message()
                .await
                .expect("We should be able to wait and get data on the rendezvous channel.");

            assert_eq!(response.status_code, StatusCode::OK);
            assert_eq!(response.data, "Hello world");
        }
    }

    #[async_test]
    async fn test_retry_mechanism() {
        let server = MockServer::start().await;
        let base_url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let rendezvous_path = mock_rendezvous_create(&server, "abcdEFG12345")
            .await
            .expect("We should be able to create a rendezvous");

        let client = HttpClient::new(reqwest::Client::new(), RequestConfig::new().disable_retry());

        let mut alice = Channel::create_outbound(client, &base_url)
            .await
            .expect("We should be able to create an outbound rendezvous channel");

        server
            .register(
                Mock::given(method("GET"))
                    .and(path(rendezvous_path.clone()))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "sequence_token": alice.sequence_token,
                        "data": "old data",
                        "expires_in_ms": 10_000,
                    })))
                    .up_to_n_times(1)
                    .expect(1),
            )
            .await;

        server
            .register(
                Mock::given(method("GET"))
                    .and(path(rendezvous_path.clone()))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "sequence_token": "3",
                        "data": "Hello world",
                        "expires_in_ms": 10_000,
                    })))
                    .expect(1),
            )
            .await;

        let response = alice
            .receive()
            .await
            .expect("We should be able to wait and get data on the rendezvous channel.");

        assert_eq!(response, "Hello world");
    }

    #[async_test]
    async fn test_receive_error() {
        let server = MockServer::start().await;
        let url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let rendezvous_path = mock_rendezvous_create(&server, "abcdEFG12345")
            .await
            .expect("We should be able to create a rendezvous");

        let client = HttpClient::new(reqwest::Client::new(), RequestConfig::new().disable_retry());

        let mut alice = Channel::create_outbound(client, &url)
            .await
            .expect("We should be able to create an outbound rendezvous channel");

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET"))
                        .and(path(rendezvous_path.clone()))
                        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                          "errcode": "M_NOT_FOUND",
                          "error": "No resource was found for this request.",
                        })))
                        .expect(1),
                )
                .await;

            alice.receive().await.expect_err("We should return an error if we receive a 404");
        }

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET"))
                        .and(path(rendezvous_path.clone()))
                        .respond_with(ResponseTemplate::new(504).set_body_json(json!({
                          "errcode": "M_NOT_FOUND",
                          "error": "No resource was found for this request.",
                        })))
                        .expect(1),
                )
                .await;

            alice
                .receive()
                .await
                .expect_err("We should return an error if we receive a gateway timeout");
        }
    }
}
