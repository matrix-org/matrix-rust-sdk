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

#[cfg(test)]
use matrix_sdk_base::crypto::types::qr_login::QrCodeModeData;
use matrix_sdk_base::crypto::types::qr_login::{QrCodeData, QrCodeMode};
use serde::{de::DeserializeOwned, Serialize};
use tracing::{instrument, trace};
#[cfg(test)]
use url::Url;
use vodozemac::ecies::{CheckCode, Ecies, EstablishedEcies, Message, OutboundCreationResult};
#[cfg(test)]
use vodozemac::ecies::{InboundCreationResult, InitialMessage};

use super::{
    rendezvous_channel::{InboundChannelCreationResult, RendezvousChannel},
    SecureChannelError as Error,
};
use crate::{config::RequestConfig, http_client::HttpClient};

const LOGIN_INITIATE_MESSAGE: &str = "MATRIX_QR_CODE_LOGIN_INITIATE";
const LOGIN_OK_MESSAGE: &str = "MATRIX_QR_CODE_LOGIN_OK";

#[cfg(test)]
pub(super) struct SecureChannel {
    channel: RendezvousChannel,
    qr_code_data: QrCodeData,
    ecies: Ecies,
}

// This is only used in tests because we're only supporting the new device part
// of the QR login flow. It will be needed once we support reciprocating of the
// login.
//
// It's still very much useful to have this, as we're testing the whole flow by
// mocking the reciprocation.
#[cfg(test)]
impl SecureChannel {
    pub(super) async fn new(http_client: HttpClient, homeserver_url: &Url) -> Result<Self, Error> {
        let channel = RendezvousChannel::create_outbound(http_client, homeserver_url).await?;
        let rendezvous_url = channel.rendezvous_url().to_owned();
        // We're a bit abusing the QR code data here, since we're passing the homeserver
        // URL, but for our tests this is fine.
        let mode_data = QrCodeModeData::Reciprocate { server_name: homeserver_url.to_string() };

        let ecies = Ecies::new();
        let public_key = ecies.public_key();

        let qr_code_data = QrCodeData { public_key, rendezvous_url, mode_data };

        Ok(Self { channel, qr_code_data, ecies })
    }

    pub(super) fn qr_code_data(&self) -> &QrCodeData {
        &self.qr_code_data
    }

    #[instrument(skip(self))]
    pub(super) async fn connect(mut self) -> Result<AlmostEstablishedSecureChannel, Error> {
        trace!("Trying to connect the secure channel.");

        let message = self.channel.receive().await?;
        let message = std::str::from_utf8(&message)?;
        let message = InitialMessage::decode(message)?;

        let InboundCreationResult { ecies, message } =
            self.ecies.establish_inbound_channel(&message)?;
        let message = std::str::from_utf8(&message)?;

        trace!("Received the initial secure channel message");

        if message == LOGIN_INITIATE_MESSAGE {
            let mut secure_channel = EstablishedSecureChannel { channel: self.channel, ecies };

            trace!("Sending the LOGIN OK message");

            secure_channel.send(LOGIN_OK_MESSAGE).await?;

            Ok(AlmostEstablishedSecureChannel { secure_channel })
        } else {
            Err(Error::SecureChannelMessage {
                expected: LOGIN_INITIATE_MESSAGE,
                received: message.to_owned(),
            })
        }
    }
}

/// An SecureChannel that is yet to be confirmed as with the [`CheckCode`].
/// Same deal as for the [`SecureChannel`], not used for now.
#[cfg(test)]
pub(super) struct AlmostEstablishedSecureChannel {
    secure_channel: EstablishedSecureChannel,
}

#[cfg(test)]
impl AlmostEstablishedSecureChannel {
    /// Confirm that the secure channel is indeed secure.
    ///
    /// The check code needs to be received out of band from the other side of
    /// the secure channel.
    pub(super) fn confirm(self, check_code: u8) -> Result<EstablishedSecureChannel, Error> {
        if check_code == self.secure_channel.check_code().to_digit() {
            Ok(self.secure_channel)
        } else {
            Err(Error::InvalidCheckCode)
        }
    }
}

pub(super) struct EstablishedSecureChannel {
    channel: RendezvousChannel,
    ecies: EstablishedEcies,
}

impl EstablishedSecureChannel {
    /// Establish a secure channel from a scanned QR code.
    #[instrument(skip(client))]
    pub(super) async fn from_qr_code(
        client: reqwest::Client,
        qr_code_data: &QrCodeData,
        expected_mode: QrCodeMode,
    ) -> Result<Self, Error> {
        if qr_code_data.mode() == expected_mode {
            Err(Error::InvalidIntent)
        } else {
            trace!("Attempting to create a new inbound secure channel from a QR code.");

            let client = HttpClient::new(client, RequestConfig::short_retry());
            let ecies = Ecies::new();

            // Let's establish an outbound ECIES channel, the other side won't know that
            // it's talking to us, the device that scanned the QR code, until it
            // receives and successfully decrypts the initial message. We're here encrypting
            // the `LOGIN_INITIATE_MESSAGE`.
            let OutboundCreationResult { ecies, message } = ecies.establish_outbound_channel(
                qr_code_data.public_key,
                LOGIN_INITIATE_MESSAGE.as_bytes(),
            )?;

            // The other side has crated a rendezvous channel, we're going to connect to it
            // and send this initial encrypted message through it. The initial message on
            // the rendezvous channel will have an empty body, so we can just
            // drop it.
            let InboundChannelCreationResult { mut channel, .. } =
                RendezvousChannel::create_inbound(client, &qr_code_data.rendezvous_url).await?;

            trace!(
                "Received the initial message from the rendezvous channel, sending the LOGIN \
                 INITIATE message"
            );

            // Now we're sending the encrypted message through the rendezvous channel to the
            // other side.
            let encoded_message = message.encode().as_bytes().to_vec();
            channel.send(encoded_message).await?;

            trace!("Waiting for the LOGIN OK message");

            // We can create our EstablishedSecureChannel struct now and use the
            // convenient helpers which transparently decrypt on receival.
            let mut ret = Self { channel, ecies };
            let response = ret.receive().await?;

            trace!("Received the LOGIN OK message, maybe.");

            if response == LOGIN_OK_MESSAGE {
                Ok(ret)
            } else {
                Err(Error::SecureChannelMessage { expected: LOGIN_OK_MESSAGE, received: response })
            }
        }
    }

    /// Get the [`CheckCode`] which can be used to, out of band, verify that
    /// both sides of the channel are indeed communicating with each other and
    /// not with a 3rd party.
    pub(super) fn check_code(&self) -> &CheckCode {
        self.ecies.check_code()
    }

    /// Send the given message over to the other side.
    ///
    /// The message will be encrypted before it is sent over the rendezvous
    /// channel.
    pub(super) async fn send_json(&mut self, message: impl Serialize) -> Result<(), Error> {
        let message = serde_json::to_string(&message)?;
        self.send(&message).await
    }

    /// Attempt to receive a message from the channel.
    ///
    /// The message will be decrypted after it has been received over the
    /// rendezvous channel.
    pub(super) async fn receive_json<D: DeserializeOwned>(&mut self) -> Result<D, Error> {
        let message = self.receive().await?;
        Ok(serde_json::from_str(&message)?)
    }

    async fn send(&mut self, message: &str) -> Result<(), Error> {
        let message = self.ecies.encrypt(message.as_bytes());
        let message = message.encode();

        Ok(self.channel.send(message.as_bytes().to_vec()).await?)
    }

    async fn receive(&mut self) -> Result<String, Error> {
        let message = self.channel.receive().await?;
        let ciphertext = std::str::from_utf8(&message)?;
        let message = Message::decode(ciphertext)?;

        let decrypted = self.ecies.decrypt(&message)?;

        Ok(String::from_utf8(decrypted).map_err(|e| e.utf8_error())?)
    }
}

#[cfg(test)]
pub(super) mod test {
    use std::sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    };

    use matrix_sdk_base::crypto::types::qr_login::QrCodeMode;
    use matrix_sdk_test::async_test;
    use serde_json::json;
    use similar_asserts::assert_eq;
    use url::Url;
    use wiremock::{
        matchers::{method, path},
        Mock, MockGuard, MockServer, ResponseTemplate,
    };

    use super::{EstablishedSecureChannel, SecureChannel};
    use crate::http_client::HttpClient;

    #[allow(dead_code)]
    pub struct MockedRendezvousServer {
        pub homeserver_url: Url,
        pub rendezvous_url: Url,
        content: Arc<Mutex<Option<String>>>,
        etag: Arc<AtomicU8>,
        post_guard: MockGuard,
        put_guard: MockGuard,
        get_guard: MockGuard,
    }

    impl MockedRendezvousServer {
        pub async fn new(server: &MockServer, location: &str) -> Self {
            let content: Arc<Mutex<Option<String>>> = Mutex::default().into();
            let etag = Arc::new(AtomicU8::new(0));

            let homeserver_url = Url::parse(&server.uri())
                .expect("We should be able to parse the example homeserver");

            let rendezvous_url = homeserver_url
                .join(location)
                .expect("We should be able to create a rendezvous URL");

            let post_guard = server
                .register_as_scoped(
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

            let put_guard = server
                .register_as_scoped(
                    Mock::given(method("PUT")).and(path("/abcdEFG12345")).respond_with({
                        let content = content.clone();
                        let etag = etag.clone();

                        move |request: &wiremock::Request| {
                            *content.lock().unwrap() =
                                Some(String::from_utf8(request.body.clone()).unwrap());
                            let current_etag = etag.fetch_add(1, Ordering::SeqCst);

                            ResponseTemplate::new(200)
                                .append_header("ETag", (current_etag + 2).to_string())
                                .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                        }
                    }),
                )
                .await;

            let get_guard = server
                .register_as_scoped(
                    Mock::given(method("GET")).and(path("/abcdEFG12345")).respond_with({
                        let content = content.clone();
                        let etag = etag.clone();

                        move |request: &wiremock::Request| {
                            let requested_etag = request.headers.get("if-none-match").map(|etag| {
                                str::parse::<u8>(std::str::from_utf8(etag.as_bytes()).unwrap())
                                    .unwrap()
                            });

                            let mut content = content.lock().unwrap();
                            let current_etag = etag.load(Ordering::SeqCst);

                            if requested_etag == Some(current_etag) || requested_etag.is_none() {
                                let content = content.take();

                                ResponseTemplate::new(200)
                                    .append_header("ETag", (current_etag).to_string())
                                    .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                    .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                                    .set_body_string(content.unwrap_or_default())
                            } else {
                                let etag = requested_etag.unwrap_or_default();

                                ResponseTemplate::new(304)
                                    .append_header("ETag", etag.to_string())
                                    .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                    .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                            }
                        }
                    }),
                )
                .await;

            Self { content, etag, post_guard, put_guard, get_guard, homeserver_url, rendezvous_url }
        }
    }

    #[async_test]
    async fn test_creation() {
        let server = MockServer::start().await;
        let rendezvous_server = MockedRendezvousServer::new(&server, "abcdEFG12345").await;

        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let alice = SecureChannel::new(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Alice should be able to create a secure channel.");

        let qr_code_data = alice.qr_code_data().clone();

        let bob_task = tokio::spawn(async move {
            EstablishedSecureChannel::from_qr_code(
                reqwest::Client::new(),
                &qr_code_data,
                QrCodeMode::Login,
            )
            .await
            .expect("Bob should be able to fully establish the secure channel.")
        });

        let alice_task = tokio::spawn(async move {
            alice
                .connect()
                .await
                .expect("Alice should be able to connect the established secure channel")
        });

        let bob = bob_task.await.unwrap();
        let alice = alice_task.await.unwrap();

        assert_eq!(alice.secure_channel.check_code(), bob.check_code());

        let alice = alice
            .confirm(bob.check_code().to_digit())
            .expect("Alice should be able to confirm the established secure channel.");

        assert_eq!(bob.channel.rendezvous_url(), alice.channel.rendezvous_url());
    }
}
