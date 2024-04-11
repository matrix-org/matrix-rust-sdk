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

use http::StatusCode;
use url::Url;

use super::requests;
use crate::{http_client::HttpClient, HttpError};

pub type Etag = String;

pub(super) struct RendezvousChannel {
    client: HttpClient,
    rendezvous_server: Url,
    rendezvous_location: String,
    etag: Etag,
}

pub struct InboundChannelCreationResult {
    pub channel: RendezvousChannel,
    pub initial_message: RendezvousMessage,
}

pub struct RendezvousMessage {
    pub status_code: StatusCode,
    pub body: Vec<u8>,
    pub content_type: String,
}

impl RendezvousChannel {
    pub(super) async fn create_outbound(
        client: HttpClient,
        rendezvous_server: &Url,
    ) -> Result<Self, HttpError> {
        let request = self::requests::create_rendezvous::Request::default();
        let response = client
            .send(request, None, rendezvous_server.to_string(), None, &[], Default::default())
            .await?;

        // TODO: FIX THE DISCOVERY HERE.
        let rendezvous_server = Url::parse("https://rendezvous.lab.element.dev").unwrap();
        let rendezvous_location = response.location;
        let etag = response.etag;

        Ok(Self { client, rendezvous_server, rendezvous_location, etag })
    }

    pub(super) async fn create_inbound(
        client: HttpClient,
        rendezvous_server: &Url,
        rendezvous_location: &str,
    ) -> Result<InboundChannelCreationResult, HttpError> {
        let rendezvous_server = rendezvous_server.to_owned();
        let rendezvous_location = rendezvous_location.to_owned();

        // Receive the initial message, which is empty? But we need the ETAG to fully
        // establish the rendezvous channel.
        let (status_code, response) = Self::receive_data_helper(
            &client,
            None,
            &rendezvous_server,
            rendezvous_location.clone(),
        )
        .await?;

        let etag = response.etag.clone();

        let initial_message = RendezvousMessage {
            status_code,
            body: response.body,
            content_type: response.content_type.unwrap_or_else(|| "application/octet".to_owned()),
        };

        let channel = Self { client, rendezvous_server, rendezvous_location, etag };

        Ok(InboundChannelCreationResult { channel, initial_message })
    }

    pub(super) fn rendezvous_server(&self) -> &Url {
        &self.rendezvous_server
    }

    pub(super) fn rendezvous_url(&self) -> Url {
        let rendezvous_path = self::requests::create_rendezvous::METADATA
            .make_endpoint_url(&[], self.rendezvous_server.as_str(), &[], "")
            .expect("We should be able to build the rendezvous path");

        let rendezvous_url = self
            .rendezvous_server
            .join(&rendezvous_path)
            .unwrap()
            .join(&self.rendezvous_location)
            .unwrap();

        rendezvous_url
    }

    async fn receive_data_helper(
        client: &HttpClient,
        etag: Option<String>,
        rendezvous_server: &Url,
        rendezvous_session: String,
    ) -> Result<(StatusCode, requests::receive_rendezvous::Response), HttpError> {
        let request = requests::receive_rendezvous::Request { rendezvous_session, etag };
        let rendezvous_server = rendezvous_server.to_string();

        let ret = client
            .send_with_status_code(request, None, rendezvous_server, None, &[], Default::default())
            .await?;

        Ok(ret)
    }

    pub(super) async fn receive_data(&mut self) -> Result<RendezvousMessage, HttpError> {
        let etag = Some(self.etag.clone());

        let (status_code, response) = Self::receive_data_helper(
            &self.client,
            etag,
            &self.rendezvous_server,
            self.rendezvous_location.to_owned(),
        )
        .await?;

        self.etag = response.etag.clone();

        let message = RendezvousMessage {
            status_code,
            body: response.body,
            content_type: response.content_type.unwrap_or_else(|| "application/octet".to_owned()),
        };

        Ok(message)
    }

    pub(super) async fn send_data(
        &mut self,
        body: Vec<u8>,
        content_type: Option<&str>,
    ) -> Result<(), HttpError> {
        let etag = self.etag.clone();

        let request = requests::send_rendezvous::Request {
            rendezvous_session: self.rendezvous_location.to_owned(),
            body,
            etag,
            content_type: content_type.map(ToOwned::to_owned),
        };

        let rendezvous_server = self.rendezvous_server.to_string();

        let response = self
            .client
            .send(request, None, rendezvous_server, None, &[], Default::default())
            .await?;

        self.etag = response.etag;

        Ok(())
    }
}
#[cfg(test)]
mod test {
    use matrix_sdk_test::async_test;
    use similar_asserts::assert_eq;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::config::RequestConfig;

    #[async_test]
    async fn creation() {
        let server = MockServer::start().await;

        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/_matrix/client/unstable/org.matrix.msc4108/rendezvous"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .append_header("Location", "abcdEFG12345")
                            .append_header("X-Max-Bytes", "10240")
                            .append_header("ETag", "VmbxF13QDusTgOCt8aoa0d2PQcnBOXeIxEqhw5aQ03o=")
                            .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                            .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT"),
                    ),
            )
            .await;

        let url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");

        let client = HttpClient::new(reqwest::Client::new(), RequestConfig::short_retry());

        let alice = RendezvousChannel::create_outbound(client, &url)
            .await
            .expect("We should be able to create an outbound rendezvous channel");

        let mut bob = {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET"))
                        .and(path(
                            "/_matrix/client/unstable/org.matrix.msc4108/rendezvous/abcdEFG12345",
                        ))
                        .respond_with(
                            ResponseTemplate::new(200)
                                .append_header("Content-Type", "application/octet-stream")
                                .append_header("ETag", "1")
                                .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT"),
                        ),
                )
                .await;

            let client = HttpClient::new(reqwest::Client::new(), RequestConfig::short_retry());
            let InboundChannelCreationResult { channel: bob, initial_message: _ } =
                RendezvousChannel::create_inbound(client, &url, &alice.rendezvous_location)
                    .await
                    .expect("");

            assert_eq!(alice.rendezvous_server(), bob.rendezvous_server());
            assert_eq!(alice.rendezvous_url(), bob.rendezvous_url());

            bob
        };

        {
            let _scope = server
                .register_as_scoped(
                    Mock::given(method("GET"))
                        .and(path(
                            "/_matrix/client/unstable/org.matrix.msc4108/rendezvous/abcdEFG12345",
                        ))
                        .respond_with(
                            ResponseTemplate::new(304)
                                .append_header("ETag", "1")
                                .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT"),
                        ),
                )
                .await;

            let response = bob.receive_data().await.expect("");
            assert_eq!(response.status_code, StatusCode::NOT_MODIFIED);
        }
    }
}
