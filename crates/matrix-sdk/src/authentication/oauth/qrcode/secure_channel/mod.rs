// Copyright 2024, 2026 The Matrix.org Foundation C.I.C.
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

use crypto_channel::*;
use matrix_sdk_base::crypto::types::qr_login::{
    Msc4108IntentData, QrCodeData, QrCodeIntent, QrCodeIntentData,
};
use serde::{Serialize, de::DeserializeOwned};
use tracing::{instrument, trace};
use url::Url;
use vodozemac::{
    ecies::{CheckCode, Ecies, EstablishedEcies, InboundCreationResult, OutboundCreationResult},
    hpke::{
        BidirectionalCreationResult, HpkeSenderChannel, InitialResponse, RecipientCreationResult,
        SenderCreationResult, UnidirectionalSenderChannel,
    },
};

use super::{
    SecureChannelError as Error,
    rendezvous_channel::{InboundChannelCreationResult, RendezvousChannel, RendezvousInfo},
};
use crate::{
    authentication::oauth::qrcode::{DecryptionError, MessageDecodeError},
    config::RequestConfig,
    http_client::HttpClient,
};
mod crypto_channel;

const LOGIN_INITIATE_MESSAGE: &str = "MATRIX_QR_CODE_LOGIN_INITIATE";
const LOGIN_OK_MESSAGE: &str = "MATRIX_QR_CODE_LOGIN_OK";

pub(super) struct SecureChannel {
    channel: RendezvousChannel,
    qr_code_data: QrCodeData,
    crypto_channel: CryptoChannel,
}

impl SecureChannel {
    /// Create a new secure channel to request a login with.
    pub(super) async fn login(
        http_client: HttpClient,
        homeserver_url: &Url,
        msc_4388: bool,
    ) -> Result<Self, Error> {
        let channel =
            RendezvousChannel::create_outbound(http_client, homeserver_url, msc_4388).await?;

        let (crypto_channel, qr_code_data) = match channel.rendezvous_info() {
            RendezvousInfo::Msc4108 { rendezvous_url } => {
                let intent_data = Msc4108IntentData::Login;
                let crypto_channel = CryptoChannel::new_ecies();

                let qr_code_data = QrCodeData::new_msc4108(
                    crypto_channel.public_key(),
                    rendezvous_url.clone(),
                    intent_data,
                );

                (crypto_channel, qr_code_data)
            }
            RendezvousInfo::Msc4388 { rendezvous_id, .. } => {
                let crypto_channel = CryptoChannel::new_hpke();

                let qr_code_data = QrCodeData::new_msc4388(
                    crypto_channel.public_key(),
                    rendezvous_id.to_owned(),
                    homeserver_url.clone(),
                    QrCodeIntent::Login,
                );

                (crypto_channel, qr_code_data)
            }
        };

        Ok(Self { channel, qr_code_data, crypto_channel })
    }

    /// Create a new secure channel to reciprocate an existing login with.
    pub(super) async fn reciprocate(
        http_client: HttpClient,
        homeserver_url: &Url,
        msc_4388: bool,
    ) -> Result<Self, Error> {
        let mut channel = SecureChannel::login(http_client, homeserver_url, msc_4388).await?;

        match channel.channel.rendezvous_info() {
            RendezvousInfo::Msc4108 { rendezvous_url } => {
                let mode_data =
                    Msc4108IntentData::Reciprocate { server_name: homeserver_url.to_string() };

                channel.qr_code_data = QrCodeData::new_msc4108(
                    channel.crypto_channel.public_key(),
                    rendezvous_url.clone(),
                    mode_data,
                );
            }
            RendezvousInfo::Msc4388 { rendezvous_id, .. } => {
                channel.qr_code_data = QrCodeData::new_msc4388(
                    channel.crypto_channel.public_key(),
                    rendezvous_id.to_owned(),
                    homeserver_url.clone(),
                    QrCodeIntent::Reciprocate,
                );
            }
        }

        Ok(channel)
    }

    pub(super) fn qr_code_data(&self) -> &QrCodeData {
        &self.qr_code_data
    }

    #[instrument(skip(self))]
    pub(super) async fn connect(mut self) -> Result<AlmostEstablishedSecureChannel, Error> {
        trace!("Trying to connect the secure channel.");

        let aad = self.channel.additional_authenticated_data().unwrap_or_default();
        let message = self.channel.receive().await?;
        let result = self.crypto_channel.establish_inbound_channel(&message, &aad)?;

        let message = std::str::from_utf8(result.plaintext()).map_err(MessageDecodeError::from)?;

        trace!("Received the initial secure channel message");

        if message == LOGIN_INITIATE_MESSAGE {
            let secure_channel = match result {
                CryptoChannelCreationResult::Ecies(InboundCreationResult { ecies, .. }) => {
                    let crypto_channel = EstablishedCryptoChannel::Ecies(ecies);

                    let mut secure_channel =
                        EstablishedSecureChannel { channel: self.channel, crypto_channel };

                    trace!("Sending the LOGIN OK message");

                    secure_channel.send(LOGIN_OK_MESSAGE).await?;
                    secure_channel
                }
                CryptoChannelCreationResult::Hpke(RecipientCreationResult { channel, .. }) => {
                    let aad = self.channel.additional_authenticated_data().unwrap_or_default();
                    let BidirectionalCreationResult { channel, message } =
                        channel.establish_bidirectional_channel(LOGIN_OK_MESSAGE.as_bytes(), &aad);

                    self.channel.send(message.encode()).await?;

                    let crypto_channel = EstablishedCryptoChannel::Hpke(channel);

                    EstablishedSecureChannel { channel: self.channel, crypto_channel }
                }
            };

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
pub(super) struct AlmostEstablishedSecureChannel {
    secure_channel: EstablishedSecureChannel,
}

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
    crypto_channel: EstablishedCryptoChannel,
}

impl EstablishedSecureChannel {
    /// Establish a secure channel from a scanned QR code.
    #[instrument(skip(client))]
    pub(super) async fn from_qr_code(
        client: reqwest::Client,
        qr_code_data: &QrCodeData,
        expected_mode: QrCodeIntent,
    ) -> Result<Self, Error> {
        enum ChannelType {
            Ecies(EstablishedEcies),
            Hpke(UnidirectionalSenderChannel),
        }

        if qr_code_data.intent() == expected_mode {
            Err(Error::InvalidIntent)
        } else {
            trace!("Attempting to create a new inbound secure channel from a QR code.");

            let client = HttpClient::new(client, RequestConfig::short_retry());

            // The other side has crated a rendezvous channel, we're going to connect to it
            // and send this initial encrypted message through it. The initial message on
            // the rendezvous channel will have an empty body, so we can just
            // drop it.
            let mut channel = match qr_code_data.intent_data() {
                QrCodeIntentData::Msc4108 { rendezvous_url, .. } => {
                    let InboundChannelCreationResult { channel, .. } =
                        RendezvousChannel::create_inbound(client, rendezvous_url).await?;
                    channel
                }
                QrCodeIntentData::Msc4388 { rendezvous_id, base_url } => {
                    let InboundChannelCreationResult { channel, .. } =
                        RendezvousChannel::create_inbound_msc4388(client, base_url, rendezvous_id)
                            .await?;
                    channel
                }
            };

            // Let's establish an outbound crypto channel, the other side won't know that
            // it's talking to us, the device that scanned the QR code, until it
            // receives and successfully decrypts the initial message. We're here encrypting
            // the `LOGIN_INITIATE_MESSAGE`.
            let (crypto_channel, encoded_message) = match qr_code_data.intent_data() {
                QrCodeIntentData::Msc4108 { .. } => {
                    let ecies = Ecies::new();

                    let OutboundCreationResult { ecies, message } = ecies
                        .establish_outbound_channel(
                            qr_code_data.public_key(),
                            LOGIN_INITIATE_MESSAGE.as_bytes(),
                        )
                        .map_err(DecryptionError::from)?;
                    (ChannelType::Ecies(ecies), message.encode())
                }
                QrCodeIntentData::Msc4388 { .. } => {
                    let aad = channel.additional_authenticated_data().unwrap_or_default();

                    let SenderCreationResult { channel, message } = HpkeSenderChannel::new()
                        .establish_channel(
                            qr_code_data.public_key(),
                            LOGIN_INITIATE_MESSAGE.as_bytes(),
                            &aad,
                        );
                    (ChannelType::Hpke(channel), message.encode())
                }
            };

            trace!(
                "Received the initial message from the rendezvous channel, sending the LOGIN \
                     INITIATE message"
            );

            // Now we're sending the encrypted message through the rendezvous channel to the
            // other side.
            channel.send(encoded_message).await?;

            trace!("Waiting for the LOGIN OK message");

            let (response, channel) = match crypto_channel {
                ChannelType::Ecies(crypto_channel) => {
                    // We can create our EstablishedSecureChannel struct now and use the
                    // convenient helpers which transparently decrypt on receival.
                    let crypto_channel = EstablishedCryptoChannel::Ecies(crypto_channel);
                    let mut channel = Self { channel, crypto_channel };

                    let response = channel.receive().await?;
                    (response, channel)
                }
                ChannelType::Hpke(crypto_channel) => {
                    let aad = channel.additional_authenticated_data().unwrap_or_default();
                    let response = channel.receive().await?;
                    let response =
                        InitialResponse::decode(&response).map_err(MessageDecodeError::from)?;

                    let BidirectionalCreationResult { channel: crypto_channel, message } =
                        crypto_channel
                            .establish_bidirectional_channel(&response, &aad)
                            .map_err(DecryptionError::from)?;
                    let response = String::from_utf8(message)
                        .map_err(|e| MessageDecodeError::from(e.utf8_error()))?;
                    let crypto_channel = EstablishedCryptoChannel::Hpke(crypto_channel);

                    // We can create our EstablishedSecureChannel struct now and use the
                    // convenient helpers which transparently decrypt on receival.
                    let channel = Self { channel, crypto_channel };

                    // We can create our EstablishedSecureChannel struct now and use the
                    // convenient helpers which transparently decrypt on receival.
                    (response, channel)
                }
            };

            trace!("Received the LOGIN OK message, maybe.");

            if response == LOGIN_OK_MESSAGE {
                Ok(channel)
            } else {
                Err(Error::SecureChannelMessage { expected: LOGIN_OK_MESSAGE, received: response })
            }
        }
    }

    /// Get the [`CheckCode`] which can be used to, out of band, verify that
    /// both sides of the channel are indeed communicating with each other and
    /// not with a 3rd party.
    pub(super) fn check_code(&self) -> &CheckCode {
        self.crypto_channel.check_code()
    }

    /// Send the given message over to the other side.
    ///
    /// The message will be encrypted before it is sent over the rendezvous
    /// channel.
    pub(super) async fn send_json(&mut self, message: impl Serialize) -> Result<(), Error> {
        let message = serde_json::to_string(&message).map_err(MessageDecodeError::from)?;
        self.send(&message).await
    }

    /// Attempt to receive a message from the channel.
    ///
    /// The message will be decrypted after it has been received over the
    /// rendezvous channel.
    pub(super) async fn receive_json<D: DeserializeOwned>(&mut self) -> Result<D, Error> {
        let message = self.receive().await?;
        Ok(serde_json::from_str(&message).map_err(MessageDecodeError::from)?)
    }

    async fn send(&mut self, message: &str) -> Result<(), Error> {
        let aad = self.channel.additional_authenticated_data().unwrap_or_default();

        let message = self.crypto_channel.seal(message, &aad);
        Ok(self.channel.send(message).await?)
    }

    async fn receive(&mut self) -> Result<String, Error> {
        let aad = self.channel.additional_authenticated_data().unwrap_or_default();

        let message = self.channel.receive().await?;
        let decrypted = self.crypto_channel.open(&message, &aad)?;

        Ok(decrypted)
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
pub(super) mod test {
    use std::{
        sync::{
            Arc, Mutex,
            atomic::{AtomicU8, Ordering},
        },
        time::Duration,
    };

    use matrix_sdk_base::crypto::types::qr_login::QrCodeIntent;
    use matrix_sdk_common::executor::spawn;
    use matrix_sdk_test::async_test;
    use ruma::time::Instant;
    use serde::Deserialize;
    use serde_json::json;
    use similar_asserts::assert_eq;
    use tracing::trace;
    use url::Url;
    use wiremock::{
        Mock, MockGuard, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::{EstablishedSecureChannel, SecureChannel};
    use crate::http_client::HttpClient;

    #[allow(dead_code)]
    pub struct MockedRendezvousServer {
        pub homeserver_url: Url,
        pub rendezvous_url: Url,
        expiration: Duration,
        content: Arc<Mutex<Option<String>>>,
        created: Arc<Mutex<Option<Instant>>>,
        etag: Arc<AtomicU8>,
        post_guard: MockGuard,
        put_guard: MockGuard,
        get_guard: MockGuard,
    }

    impl MockedRendezvousServer {
        pub async fn new(
            server: &MockServer,
            location: &str,
            expiration: Duration,
            msc_4388: bool,
        ) -> Self {
            if msc_4388 {
                Self::new_msc4388(server, expiration).await
            } else {
                Self::new_msc4108(server, location, expiration).await
            }
        }

        pub async fn new_msc4108(
            server: &MockServer,
            location: &str,
            expiration: Duration,
        ) -> Self {
            let content: Arc<Mutex<Option<String>>> = Mutex::default().into();
            let created: Arc<Mutex<Option<Instant>>> = Mutex::default().into();
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
                        .respond_with({
                            *created.lock().unwrap() = Some(Instant::now());

                            ResponseTemplate::new(200)
                                .append_header("X-Max-Bytes", "10240")
                                .append_header("ETag", "1")
                                .append_header("Expires", "Wed, 07 Sep 2022 14:28:51 GMT")
                                .append_header("Last-Modified", "Wed, 07 Sep 2022 14:27:51 GMT")
                                .set_body_json(json!({
                                    "url": rendezvous_url,
                                }))
                        }),
                )
                .await;

            let put_guard = server
                .register_as_scoped(
                    Mock::given(method("PUT")).and(path("/abcdEFG12345")).respond_with({
                        let content = content.clone();
                        let created = created.clone();
                        let etag = etag.clone();

                        move |request: &wiremock::Request| {
                            // Fail the request if the session has expired.
                            if created.lock().unwrap().unwrap().elapsed() > expiration {
                                return ResponseTemplate::new(404).set_body_json(json!({
                                    "errcode": "M_NOT_FOUND",
                                    "error": "This rendezvous session does not exist.",
                                }));
                            }

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
                        let created = created.clone();
                        let etag = etag.clone();

                        move |request: &wiremock::Request| {
                            // Fail the request if the session has expired.
                            if created.lock().unwrap().unwrap().elapsed() > expiration {
                                return ResponseTemplate::new(404).set_body_json(json!({
                                    "errcode": "M_NOT_FOUND",
                                    "error": "This rendezvous session does not exist.",
                                }));
                            }

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

            Self {
                expiration,
                content,
                created,
                etag,
                post_guard,
                put_guard,
                get_guard,
                homeserver_url,
                rendezvous_url,
            }
        }

        pub async fn new_msc4388(server: &MockServer, expiration: Duration) -> Self {
            #[derive(Debug, Deserialize)]
            struct PutContent {
                #[allow(dead_code)]
                sequence_token: String,
                data: String,
            }

            const RENDEZVOUS_ID: &str = "abcdEFG12345";

            let content: Arc<Mutex<Option<String>>> = Mutex::default().into();
            let created: Arc<Mutex<Option<Instant>>> = Mutex::default().into();
            let sequence_token = Arc::new(AtomicU8::new(0));

            let homeserver_url = Url::parse(&server.uri())
                .expect("We should be able to parse the example homeserver");

            let rendezvous_url = homeserver_url
                .join(RENDEZVOUS_ID)
                .expect("We should be able to create a rendezvous URL");

            let post_guard = server
                .register_as_scoped(
                    Mock::given(method("POST"))
                        .and(path("/_matrix/client/unstable/io.element.msc4388/rendezvous"))
                        .respond_with({
                            *created.lock().unwrap() = Some(Instant::now());

                            trace!("Creating a new rendezvous channel ID: {RENDEZVOUS_ID}");

                            ResponseTemplate::new(200).set_body_json(json!({
                                "id": RENDEZVOUS_ID,
                                "sequence_token": "0",
                                "expires_in_ms": 100_000,
                            }))
                        }),
                )
                .await;

            let put_guard = server
                .register_as_scoped(
                    Mock::given(method("PUT"))
                        .and(path(format!(
                            "/_matrix/client/unstable/io.element.msc4388/rendezvous/{RENDEZVOUS_ID}"
                        )))
                        .respond_with({
                            let content = content.clone();
                            let created = created.clone();
                            let sequence_token = sequence_token.clone();


                            move |request: &wiremock::Request| {
                                // Fail the request if the session has expired.
                                if created.lock().unwrap().unwrap().elapsed() > expiration {
                                    return ResponseTemplate::new(404).set_body_json(json!({
                                        "errcode": "M_NOT_FOUND",
                                        "error": "This rendezvous session does not exist.",
                                    }));
                                }

                                let request_content: PutContent = request.body_json().unwrap();
                                *content.lock().unwrap() = Some(request_content.data);

                                let prev_token =
                                    sequence_token.fetch_add(1, Ordering::SeqCst);

                                trace!("Putting new content into the rendezvous channel ID: {RENDEZVOUS_ID}");

                                ResponseTemplate::new(200).set_body_json(json!({
                                    "sequence_token": (prev_token + 1 ).to_string(),

                                }))
                            }
                        }),
                )
                .await;

            let get_guard = server
                .register_as_scoped(
                    Mock::given(method("GET"))
                        .and(path(format!(
                            "/_matrix/client/unstable/io.element.msc4388/rendezvous/{RENDEZVOUS_ID}"
                        )))
                        .respond_with({
                            let content = content.clone();
                            let created = created.clone();
                            let sequence_token = sequence_token.clone();

                            move |_: &wiremock::Request| {
                                // Fail the request if the session has expired.
                                if created.lock().unwrap().unwrap().elapsed() > expiration {
                                    return ResponseTemplate::new(404).set_body_json(json!({
                                        "errcode": "M_NOT_FOUND",
                                        "error": "This rendezvous session does not exist.",
                                    }));
                                }

                                let content = content.lock().unwrap();
                                let current_sequence_token = sequence_token.load(Ordering::SeqCst);

                                let content = content.clone();

                                ResponseTemplate::new(200).set_body_json(json!({
                                    "data": content.unwrap_or_default(),
                                    "sequence_token": current_sequence_token.to_string(),
                                    "expires_in_ms": 100_000,
                                }))
                            }
                        }),
                )
                .await;

            Self {
                expiration,
                content,
                created,
                etag: sequence_token,
                post_guard,
                put_guard,
                get_guard,
                homeserver_url,
                rendezvous_url,
            }
        }
    }

    async fn test_creation(msc_4388: bool) {
        let server = MockServer::start().await;
        let rendezvous_server =
            MockedRendezvousServer::new(&server, "abcdEFG12345", Duration::MAX, msc_4388).await;

        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let alice = SecureChannel::reciprocate(client, &rendezvous_server.homeserver_url, msc_4388)
            .await
            .expect("Alice should be able to create a secure channel.");

        let qr_code_data = alice.qr_code_data().clone();

        let bob_task = spawn(async move {
            EstablishedSecureChannel::from_qr_code(
                reqwest::Client::new(),
                &qr_code_data,
                QrCodeIntent::Login,
            )
            .await
            .expect("Bob should be able to fully establish the secure channel.")
        });

        let alice_task = spawn(async move {
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

        assert_eq!(bob.channel.rendezvous_info(), alice.channel.rendezvous_info());
    }

    #[async_test]
    async fn test_creation_msc4388() {
        test_creation(true).await;
    }

    #[async_test]
    async fn test_creation_msc4108() {
        test_creation(false).await;
    }
}
