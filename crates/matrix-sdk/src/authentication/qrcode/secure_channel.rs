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

use http::StatusCode;
use matrix_sdk_base::crypto::qr_login::{QrCodeData, QrCodeMode, QrCodeModeData};
use ruma::api::client::error::ErrorKind;
use url::Url;
use vodozemac::secure_channel::{
    CheckCode, EstablishedSecureChannel as EstablishedEcies, InitialMessage, Message,
    SecureChannel as Ecies,
};

use super::{
    rendezvous_channel::{InboundChannelCreationResult, RendezvousChannel},
    Error,
};
use crate::{config::RequestConfig, http_client::HttpClient};

pub struct SecureChannel {
    channel: RendezvousChannel,
    qr_code_data: QrCodeData,
    ecies: Ecies,
}

const RECEIVE_TIMEOUT: Duration = Duration::from_secs(1);
const LOGIN_INITIATE_MESSAGE: &str = "MATRIX_QR_CODE_LOGIN_INITIATE";
const LOGIN_OK_MESSAGE: &str = "MATRIX_QR_CODE_LOGIN_OK";
const TEXT_PLAIN_CONTENT_TYPE: &str = "text/plain";

impl SecureChannel {
    async fn new_helper(
        client: HttpClient,
        rendezvous_server: Url,
        mode: QrCodeModeData,
    ) -> Result<Self, Error> {
        let channel = RendezvousChannel::create_outbound(client, &rendezvous_server).await?;
        let rendezvous_url = channel.rendezvous_url();

        let ecies = Ecies::new();
        let public_key = ecies.public_key();

        let qr_code_data = QrCodeData { public_key, rendezvous_url, mode };

        Ok(Self { channel, qr_code_data, ecies })
    }

    pub async fn login(client: reqwest::Client, rendezvous_url: Url) -> Result<Self, Error> {
        let client = HttpClient::new(client, RequestConfig::short_retry());
        let mode = QrCodeModeData::Login;
        Self::new_helper(client, rendezvous_url, mode).await
    }

    pub(crate) async fn reciprocate(
        http_client: HttpClient,
        homeserver_url: &Url,
    ) -> Result<Self, Error> {
        let mode = QrCodeModeData::Reciprocate { homeserver_url: homeserver_url.clone() };
        Self::new_helper(http_client, homeserver_url.clone(), mode).await
    }

    pub fn qr_code_data(&self) -> &QrCodeData {
        &self.qr_code_data
    }

    async fn receive_data(&mut self) -> Result<Vec<u8>, Error> {
        loop {
            match self.channel.receive_data().await {
                Ok(response) => {
                    if response.status_code == StatusCode::OK
                        && &response.content_type == TEXT_PLAIN_CONTENT_TYPE
                    {
                        return Ok(response.body);
                    }
                }
                Err(e) => {
                    // If it's a permanent error, return an error, if it's a not changed error,
                    // sleep and try again.
                    //
                    if let Some(kind) = e.client_api_error_kind() {
                        if *kind == ErrorKind::NotFound {
                            return Err(e.into());
                        }
                    }
                }
            }

            tokio::time::sleep(RECEIVE_TIMEOUT).await;
        }
    }

    pub async fn connect(mut self) -> Result<AlmostEstablishedSecureChannel, Error> {
        let message = self.receive_data().await?;
        let message = String::from_utf8(message).unwrap();
        let message = InitialMessage::decode(&message).unwrap();

        let result = self.ecies.create_inbound_channel(&message)?;

        let message = String::from_utf8(result.message).unwrap();

        if message == LOGIN_INITIATE_MESSAGE {
            let mut secure_channel =
                EstablishedSecureChannel { channel: self.channel, ecies: result.secure_channel };

            secure_channel.send(LOGIN_OK_MESSAGE).await?;

            Ok(AlmostEstablishedSecureChannel { secure_channel })
        } else {
            todo!()
        }
    }
}

pub struct AlmostEstablishedSecureChannel {
    secure_channel: EstablishedSecureChannel,
}

impl AlmostEstablishedSecureChannel {
    pub fn confirm(self, check_code: u8) -> Result<EstablishedSecureChannel, Error> {
        if check_code == self.secure_channel.check_code().to_digit() {
            Ok(self.secure_channel)
        } else {
            todo!()
        }
    }
}

pub struct EstablishedSecureChannel {
    channel: RendezvousChannel,
    ecies: EstablishedEcies,
}

impl EstablishedSecureChannel {
    pub async fn send(&mut self, message: &str) -> Result<(), Error> {
        let message = self.ecies.encrypt(message.as_bytes());
        let message = message.encode();

        Ok(self
            .channel
            .send_data(message.as_bytes().to_vec(), Some(TEXT_PLAIN_CONTENT_TYPE))
            .await?)
    }

    pub async fn receive(&mut self) -> Result<String, Error> {
        let response = self.channel.receive_data().await?;
        let ciphertext = String::from_utf8(response.body).unwrap();
        let message = Message::decode(&ciphertext).unwrap();

        let decrypted = self.ecies.decrypt(&message).unwrap();

        Ok(String::from_utf8(decrypted).unwrap())
    }

    pub fn check_code(&self) -> &CheckCode {
        self.ecies.check_code()
    }

    pub async fn from_qr_code(
        qr_code_data: &QrCodeData,
        expected_mode: QrCodeMode,
    ) -> Result<Self, Error> {
        if qr_code_data.mode.mode_identifier() == expected_mode {
            todo!("Same intent, throw error");
        }

        let client = HttpClient::new(reqwest::Client::new(), RequestConfig::short_retry());
        let ecies = Ecies::new();
        let ecies = ecies.create_outbound_channel(qr_code_data.public_key)?;

        let rendezvous_location =
            if let Some(segments) = qr_code_data.rendezvous_url.path_segments() {
                segments.last().ok_or_else(|| url::ParseError::EmptyHost)?.to_owned()
            } else {
                todo!()
            };

        let mut rendezvous_server = qr_code_data.rendezvous_url.clone();
        rendezvous_server.set_path("");
        rendezvous_server.set_query(None);

        // The initial response will have an empty body, so we can just drop it.
        let InboundChannelCreationResult { channel, initial_message: _ } =
            RendezvousChannel::create_inbound(client, &rendezvous_server, &rendezvous_location)
                .await?;

        let mut ret = Self { channel, ecies };

        ret.send(LOGIN_INITIATE_MESSAGE).await?;

        let response = ret.receive().await?;

        if response == LOGIN_OK_MESSAGE {
            Ok(ret)
        } else {
            todo!()
        }
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_base::crypto::qr_login::QrCodeMode;
    use matrix_sdk_test::async_test;
    use similar_asserts::assert_eq;
    use url::Url;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::{EstablishedSecureChannel, SecureChannel};
    use crate::http_client::HttpClient;

    #[async_test]
    async fn creation() {
        let server = MockServer::start().await;

        server
            .register(
                Mock::given(method("POST"))
                    .and(path("/_matrix/client/unstable/org.matrix.msc4108/rendezvous"))
                    .respond_with(
                        ResponseTemplate::new(201)
                            .append_header("Location", "/abcdEFG12345")
                            .append_header("X-Max-Bytes", "10240")
                            .append_header("ETag", "1")
                            .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                            .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT"),
                    ),
            )
            .await;

        server
            .register(
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

        let homeserver =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");

        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let alice = SecureChannel::reciprocate(client, &homeserver)
            .await
            .expect("We should be able to create a QR auth object");

        let bob = EstablishedSecureChannel::from_qr_code(alice.qr_code_data(), QrCodeMode::Login)
            .await
            .expect("We should be able to create a Qr auth object from QR code data");

        assert_eq!(bob.channel.rendezvous_server(), &homeserver);
    }
}
