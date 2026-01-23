// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::time::Duration;

use http::{
    HeaderMap, HeaderName, Method, StatusCode,
    header::{CONTENT_TYPE, ETAG, EXPIRES, IF_MATCH, IF_NONE_MATCH, LAST_MODIFIED},
};
use matrix_sdk_base::sleep;
use ruma::api::{
    EndpointError,
    error::{FromHttpResponseError, HeaderDeserializationError, IntoHttpError},
};
use tracing::{debug, instrument, trace};
use url::Url;

use crate::{HttpError, RumaApiError, http_client::HttpClient};

const TEXT_PLAIN_CONTENT_TYPE: &str = "text/plain";
#[cfg(test)]
const POLL_TIMEOUT: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const POLL_TIMEOUT: Duration = Duration::from_secs(1);

type Etag = String;

/// Get a header from a [`HeaderMap`] and parse it as a UTF-8 string.
fn get_header(
    header_map: &HeaderMap,
    header_name: &HeaderName,
) -> Result<String, Box<FromHttpResponseError<RumaApiError>>> {
    let header = header_map
        .get(header_name)
        .ok_or(HeaderDeserializationError::MissingHeader(ETAG.to_string()))
        .map_err(|error| Box::new(FromHttpResponseError::from(error)))?;

    let header =
        header.to_str().map_err(|error| Box::new(FromHttpResponseError::from(error)))?.to_owned();

    Ok(header)
}

/// The result of the [`RendezvousChannel::create_inbound()`] method.
pub(super) struct InboundChannelCreationResult {
    /// The connected [`RendezvousChannel`].
    pub channel: RendezvousChannel,
    /// The initial message we received when we connected to the
    /// [`RendezvousChannel`].
    ///
    /// This is currently unused, but left in for completeness sake.
    #[allow(dead_code)]
    pub initial_message: Vec<u8>,
}

struct RendezvousGetResponse {
    pub status_code: StatusCode,
    pub etag: String,
    // TODO: This is currently unused, but will be required once we implement the reciprocation of
    // a login. Left here so we don't forget about it. We should put this into the
    // [`RendezvousChannel`] struct, once we parse it into a [`SystemTime`].
    #[allow(dead_code)]
    pub expires: String,
    #[allow(dead_code)]
    pub last_modified: String,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

struct RendezvousMessage {
    pub status_code: StatusCode,
    pub body: Vec<u8>,
    pub content_type: String,
}

pub(super) struct RendezvousChannel {
    client: HttpClient,
    rendezvous_url: Url,
    etag: Etag,
}

fn response_to_error(status: StatusCode, body: Vec<u8>) -> HttpError {
    match http::Response::builder().status(status).body(body).map_err(IntoHttpError::from) {
        Ok(response) => {
            let error = FromHttpResponseError::<RumaApiError>::Server(RumaApiError::ClientApi(
                ruma::api::client::Error::from_http_response(response),
            ));

            error.into()
        }
        Err(e) => HttpError::IntoHttp(e),
    }
}

impl RendezvousChannel {
    /// Create a new outbound [`RendezvousChannel`].
    ///
    /// By outbound we mean that we're going to tell the Matrix server to create
    /// a new rendezvous session. We're going to send an initial empty message
    /// through the channel.
    pub(super) async fn create_outbound(
        client: HttpClient,
        rendezvous_server: &Url,
    ) -> Result<Self, HttpError> {
        use std::borrow::Cow;

        use ruma::api::{SupportedVersions, client::rendezvous::create_rendezvous_session};

        let request = create_rendezvous_session::unstable::Request::default();
        let response = client
            .send(
                request,
                None,
                rendezvous_server.to_string(),
                None,
                Cow::Owned(SupportedVersions {
                    versions: Default::default(),
                    features: Default::default(),
                }),
                Default::default(),
            )
            .await?;

        let rendezvous_url = response.url;
        let etag = response.etag;

        Ok(Self { client, rendezvous_url, etag })
    }

    /// Create a new inbound [`RendezvousChannel`].
    ///
    /// By inbound we mean that we're going to attempt to read an initial
    /// message from the rendezvous session on the given [`rendezvous_url`].
    pub(super) async fn create_inbound(
        client: HttpClient,
        rendezvous_url: &Url,
    ) -> Result<InboundChannelCreationResult, HttpError> {
        // Receive the initial message, which should be empty. But we need the ETAG to
        // fully establish the rendezvous channel.
        let response = Self::receive_message_impl(&client.inner, None, rendezvous_url).await?;

        let etag = response.etag.clone();

        let initial_message = RendezvousMessage {
            status_code: response.status_code,
            body: response.body,
            content_type: response.content_type.unwrap_or_else(|| "text/plain".to_owned()),
        };

        let channel = Self { client, rendezvous_url: rendezvous_url.clone(), etag };

        Ok(InboundChannelCreationResult { channel, initial_message: initial_message.body })
    }

    /// Get the URL of the rendezvous session we're using to exchange messages
    /// through the channel.
    pub(super) fn rendezvous_url(&self) -> &Url {
        &self.rendezvous_url
    }

    /// Send the given `message` through the [`RendezvousChannel`] to the other
    /// device.
    ///
    /// The message must be of the `text/plain` content type.
    #[instrument(skip_all)]
    pub(super) async fn send(&mut self, message: Vec<u8>) -> Result<(), HttpError> {
        let etag = self.etag.clone();

        let request = self
            .client
            .inner
            .request(Method::PUT, self.rendezvous_url().to_owned())
            .body(message)
            .header(IF_MATCH, etag)
            .header(CONTENT_TYPE, TEXT_PLAIN_CONTENT_TYPE);

        debug!("Sending a request to the rendezvous channel {request:?}");

        let response = request.send().await?;
        let status = response.status();

        debug!("Response for the rendezvous sending request {response:?}");

        if status.is_success() {
            // We successfully send out a message, get the ETAG and update our internal copy
            // of the ETAG.
            let etag = get_header(response.headers(), &ETAG)?;
            self.etag = etag;

            Ok(())
        } else {
            let body = response.bytes().await?;
            let error = response_to_error(status, body.to_vec());

            return Err(error);
        }
    }

    /// Attempt to receive a message from the [`RendezvousChannel`] from the
    /// other device.
    ///
    /// The content should be of the `text/plain` content type but the parsing
    /// and verification of this fact is left up to the caller.
    ///
    /// This method will wait in a loop for the channel to give us a new
    /// message.
    pub(super) async fn receive(&mut self) -> Result<Vec<u8>, HttpError> {
        loop {
            let message = self.receive_single_message().await?;

            trace!(
                status_code = %message.status_code,
                "Received data from the rendezvous channel"
            );

            if message.status_code == StatusCode::OK
                && message.content_type == TEXT_PLAIN_CONTENT_TYPE
                && !message.body.is_empty()
            {
                return Ok(message.body);
            } else if message.status_code == StatusCode::NOT_MODIFIED {
                sleep::sleep(POLL_TIMEOUT).await;
                continue;
            } else {
                let error = response_to_error(message.status_code, message.body);

                return Err(error);
            }
        }
    }

    #[instrument]
    async fn receive_message_impl(
        client: &reqwest::Client,
        etag: Option<String>,
        rendezvous_url: &Url,
    ) -> Result<RendezvousGetResponse, HttpError> {
        let mut builder = client.request(Method::GET, rendezvous_url.to_owned());

        if let Some(etag) = etag {
            builder = builder.header(IF_NONE_MATCH, etag);
        }

        let response = builder.send().await?;

        debug!("Received data from the rendezvous channel {response:?}");

        let status_code = response.status();

        if status_code.is_client_error() {
            return Err(response_to_error(status_code, response.bytes().await?.to_vec()));
        }

        let headers = response.headers();
        let etag = get_header(headers, &ETAG)?;
        let expires = get_header(headers, &EXPIRES)?;
        let last_modified = get_header(headers, &LAST_MODIFIED)?;
        let content_type = headers
            .get(CONTENT_TYPE)
            .map(|c| c.to_str().map_err(FromHttpResponseError::<RumaApiError>::from))
            .transpose()?
            .map(ToOwned::to_owned);

        let body = response.bytes().await?.to_vec();

        let response =
            RendezvousGetResponse { status_code, etag, expires, last_modified, content_type, body };

        Ok(response)
    }

    async fn receive_single_message(&mut self) -> Result<RendezvousMessage, HttpError> {
        let etag = Some(self.etag.clone());

        let RendezvousGetResponse { status_code, etag, content_type, body, .. } =
            Self::receive_message_impl(&self.client.inner, etag, &self.rendezvous_url).await?;

        // We received a response with an ETAG, put it into the copy of our etag.
        self.etag = etag;

        let message = RendezvousMessage {
            status_code,
            body,
            content_type: content_type.unwrap_or_else(|| "text/plain".to_owned()),
        };

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
        matchers::{header, method, path},
    };

    use super::*;
    use crate::config::RequestConfig;

    async fn mock_rendzvous_create(server: &MockServer, rendezvous_url: &Url) {
        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/_matrix/client/unstable/org.matrix.msc4108/rendezvous"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .append_header("X-Max-Bytes", "10240")
                            .append_header("ETag", "1")
                            .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                            .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                            .set_body_json(json!({
                                "url": rendezvous_url,
                            })),
                    ),
            )
            .await;
    }

    #[async_test]
    async fn test_creation() {
        let server = MockServer::start().await;
        let url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let rendezvous_url =
            url.join("abcdEFG12345").expect("We should be able to create a rendezvous URL");

        mock_rendzvous_create(&server, &rendezvous_url).await;

        let client = HttpClient::new(reqwest::Client::new(), RequestConfig::new().disable_retry());

        let mut alice = RendezvousChannel::create_outbound(client, &url)
            .await
            .expect("We should be able to create an outbound rendezvous channel");

        assert_eq!(
            alice.rendezvous_url(),
            &rendezvous_url,
            "Alice should have configured the rendezvous URL correctly."
        );

        assert_eq!(alice.etag, "1", "Alice should have remembered the ETAG the server gave us.");

        let mut bob = {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET")).and(path("/abcdEFG12345")).respond_with(
                        ResponseTemplate::new(200)
                            .append_header("Content-Type", "text/plain")
                            .append_header("ETag", "2")
                            .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                            .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT"),
                    ),
                )
                .await;

            let client = HttpClient::new(reqwest::Client::new(), RequestConfig::short_retry());
            let InboundChannelCreationResult { channel: bob, initial_message: _ } =
                RendezvousChannel::create_inbound(client, &rendezvous_url).await.expect(
                    "We should be able to create a rendezvous channel from a received message",
                );

            assert_eq!(alice.rendezvous_url(), bob.rendezvous_url());

            bob
        };

        assert_eq!(bob.etag, "2", "Bob should have remembered the ETAG the server gave us.");

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET"))
                        .and(path("/abcdEFG12345"))
                        .and(header("if-none-match", "1"))
                        .respond_with(
                            ResponseTemplate::new(304)
                                .append_header("ETag", "1")
                                .append_header("Content-Type", "text/plain")
                                .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT"),
                        ),
                )
                .await;

            let response = alice
                .receive_single_message()
                .await
                .expect("We should be able to wait for data on the rendezvous channel.");
            assert_eq!(response.status_code, StatusCode::NOT_MODIFIED);
        }

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("PUT"))
                        .and(path("/abcdEFG12345"))
                        .and(header("Content-Type", "text/plain"))
                        .respond_with(
                            ResponseTemplate::new(200)
                                .append_header("ETag", "1")
                                .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT"),
                        ),
                )
                .await;

            bob.send(b"Hello world".to_vec())
                .await
                .expect("We should be able to send data to the rendezouvs server.");
        }

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET"))
                        .and(path("/abcdEFG12345"))
                        .and(header("if-none-match", "1"))
                        .respond_with(
                            ResponseTemplate::new(200)
                                .append_header("ETag", "3")
                                .append_header("Content-Type", "text/plain")
                                .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                                .set_body_string("Hello world"),
                        ),
                )
                .await;

            let response = alice
                .receive_single_message()
                .await
                .expect("We should be able to wait and get data on the rendezvous channel.");

            assert_eq!(response.status_code, StatusCode::OK);
            assert_eq!(response.body, b"Hello world");
            assert_eq!(response.content_type, TEXT_PLAIN_CONTENT_TYPE);
        }
    }

    #[async_test]
    async fn test_retry_mechanism() {
        let server = MockServer::start().await;
        let url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let rendezvous_url =
            url.join("abcdEFG12345").expect("We should be able to create a rendezvous URL");
        mock_rendzvous_create(&server, &rendezvous_url).await;

        let client = HttpClient::new(reqwest::Client::new(), RequestConfig::new().disable_retry());

        let mut alice = RendezvousChannel::create_outbound(client, &url)
            .await
            .expect("We should be able to create an outbound rendezvous channel");

        server
            .register(
                Mock::given(method("GET"))
                    .and(path("/abcdEFG12345"))
                    .and(header("if-none-match", "1"))
                    .respond_with(
                        ResponseTemplate::new(304)
                            .append_header("ETag", "2")
                            .append_header("Content-Type", "text/plain")
                            .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                            .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                            .set_body_string(""),
                    )
                    .expect(1),
            )
            .await;

        server
            .register(
                Mock::given(method("GET"))
                    .and(path("/abcdEFG12345"))
                    .and(header("if-none-match", "2"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .append_header("ETag", "3")
                            .append_header("Content-Type", "text/plain")
                            .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                            .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                            .set_body_string("Hello world"),
                    )
                    .expect(1),
            )
            .await;

        let response = alice
            .receive()
            .await
            .expect("We should be able to wait and get data on the rendezvous channel.");

        assert_eq!(response, b"Hello world");
    }

    #[async_test]
    async fn test_receive_error() {
        let server = MockServer::start().await;
        let url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let rendezvous_url =
            url.join("abcdEFG12345").expect("We should be able to create a rendezvous URL");
        mock_rendzvous_create(&server, &rendezvous_url).await;

        let client = HttpClient::new(reqwest::Client::new(), RequestConfig::new().disable_retry());

        let mut alice = RendezvousChannel::create_outbound(client, &url)
            .await
            .expect("We should be able to create an outbound rendezvous channel");

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET"))
                        .and(path("/abcdEFG12345"))
                        .and(header("if-none-match", "1"))
                        .respond_with(
                            ResponseTemplate::new(404)
                                .append_header("ETag", "1")
                                .append_header("Content-Type", "text/plain")
                                .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                                .set_body_string(""),
                        )
                        .expect(1),
                )
                .await;

            alice.receive().await.expect_err("We should return an error if we receive a 404");
        }

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET"))
                        .and(path("/abcdEFG12345"))
                        .and(header("if-none-match", "1"))
                        .respond_with(
                            ResponseTemplate::new(504)
                                .append_header("ETag", "1")
                                .append_header("Content-Type", "text/plain")
                                .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                                .set_body_json(json!({
                                  "errcode": "M_NOT_FOUND",
                                  "error": "No resource was found for this request.",
                                })),
                        )
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
