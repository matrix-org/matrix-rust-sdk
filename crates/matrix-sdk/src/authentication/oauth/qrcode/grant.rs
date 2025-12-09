// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use eyeball::SharedObservable;
use futures_core::Stream;
use matrix_sdk_base::{
    boxed_into_future,
    crypto::types::{
        SecretsBundle,
        qr_login::{QrCodeData, QrCodeMode},
    },
};
use oauth2::VerificationUriComplete;
use ruma::time::Instant;
use url::Url;
#[cfg(doc)]
use vodozemac::ecies::CheckCode;

use super::{
    LoginProtocolType, QrAuthMessage,
    secure_channel::{EstablishedSecureChannel, SecureChannel},
};
use crate::{
    Client,
    authentication::oauth::qrcode::{
        CheckCodeSender, GeneratedQrProgress, LoginFailureReason, QRCodeGrantLoginError,
        QrProgress, SecureChannelError,
    },
};

async fn export_secrets_bundle(client: &Client) -> Result<SecretsBundle, QRCodeGrantLoginError> {
    let secrets_bundle = client
        .olm_machine()
        .await
        .as_ref()
        .ok_or_else(|| QRCodeGrantLoginError::MissingSecretsBackup(None))?
        .store()
        .export_secrets_bundle()
        .await?;
    Ok(secrets_bundle)
}

async fn finish_login_grant<Q>(
    client: &Client,
    channel: &mut EstablishedSecureChannel,
    device_creation_timeout: Duration,
    secrets_bundle: &SecretsBundle,
    state: &SharedObservable<GrantLoginProgress<Q>>,
) -> Result<(), QRCodeGrantLoginError> {
    // The new device registers with the authorization server and sends it a device
    // authorization authorization request.
    // -- MSC4108 OAuth 2.0 login step 2

    // We wait for the new device to send us the m.login.protocol message with the
    // device authorization grant information. -- MSC4108 OAuth 2.0 login step 3
    let message = channel.receive_json().await?;
    let QrAuthMessage::LoginProtocol { device_authorization_grant, protocol, device_id } = message
    else {
        return Err(QRCodeGrantLoginError::Unknown(
            "Receiving unexpected message when expecting LoginProtocol".to_owned(),
        ));
    };

    // We verify the selected protocol.
    // -- MSC4108 OAuth 2.0 login step 4
    if protocol != LoginProtocolType::DeviceAuthorizationGrant {
        channel
            .send_json(QrAuthMessage::LoginFailure {
                reason: LoginFailureReason::UnsupportedProtocol,
                homeserver: None,
            })
            .await?;
        return Err(QRCodeGrantLoginError::UnsupportedProtocol(protocol));
    }

    // We check that the device ID is still available.
    // -- MSC4108 OAuth 2.0 login step 4 continued
    if !matches!(client.device_exists(device_id.clone().into()).await, Ok(false)) {
        channel
            .send_json(QrAuthMessage::LoginFailure {
                reason: LoginFailureReason::DeviceAlreadyExists,
                homeserver: None,
            })
            .await?;
        return Err(QRCodeGrantLoginError::DeviceIDAlreadyInUse);
    }

    // We emit an update so that the caller can open the verification URI in a
    // system browser to consent to the login.
    // -- MSC4108 OAuth 2.0 login step 4 continued
    let verification_uri = Url::parse(
        device_authorization_grant
            .verification_uri_complete
            .map(VerificationUriComplete::into_secret)
            .unwrap_or(device_authorization_grant.verification_uri.to_string())
            .as_str(),
    )
    .map_err(|_| QRCodeGrantLoginError::UnableToCreateDevice)?;
    state.set(GrantLoginProgress::WaitingForAuth { verification_uri });

    // We send the new device the m.login.protocol_accepted message to let it know
    // that the consent process is in progress.
    // -- MSC4108 OAuth 2.0 login step 4 continued
    let message = QrAuthMessage::LoginProtocolAccepted;
    channel.send_json(&message).await?;

    // The new device displays the user code it received from the authorization
    // server and starts polling for an access token. In parallel, the user
    // consents to the new login in the browser on this device, while verifying
    // the user code displayed on the other device. -- MSC4108 OAuth 2.0 login
    // steps 5 & 6

    // We wait for the new device to send us the m.login.success message
    let message: QrAuthMessage = channel.receive_json().await?;
    let QrAuthMessage::LoginSuccess = message else {
        return Err(QRCodeGrantLoginError::Unknown(
            "Receiving unexpected message when expecting LoginSuccess".to_owned(),
        ));
    };

    // We check that the new device was created successfully, allowing for the
    // specified delay. -- MSC4108 Secret sharing and device verification step 1
    let deadline = Instant::now() + device_creation_timeout;

    loop {
        if matches!(client.device_exists(device_id.clone().into()).await, Ok(true)) {
            break;
        } else {
            // If the deadline hasn't yet passed, give it some time and retry the request.
            if Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            } else {
                // The deadline has passed. Let's fail the login process.
                channel
                    .send_json(QrAuthMessage::LoginFailure {
                        reason: LoginFailureReason::DeviceNotFound,
                        homeserver: None,
                    })
                    .await?;
                return Err(QRCodeGrantLoginError::DeviceIDAlreadyInUse);
            }
        }
    }

    // We send the new device the secrets bundle.
    // -- MSC4108 Secret sharing and device verification step 2
    state.set(GrantLoginProgress::SyncingSecrets);
    let message = QrAuthMessage::LoginSecrets(secrets_bundle.clone());
    channel.send_json(&message).await?;

    // And we're done.
    state.set(GrantLoginProgress::Done);

    Ok(())
}

/// The progress of granting the login.
#[derive(Clone, Debug, Default)]
pub enum GrantLoginProgress<Q> {
    /// We're just starting up, this is the default and initial state.
    #[default]
    Starting,
    /// The secure channel is being established by exchanging the QR code
    /// and/or [`CheckCode`].
    EstablishingSecureChannel(Q),
    /// The secure channel has been confirmed using the [`CheckCode`] and this
    /// device is waiting for the authorization to complete.
    WaitingForAuth {
        /// A URI to open in a (secure) system browser to verify the new login.
        verification_uri: Url,
    },
    /// The new device has been granted access and this device is sending the
    /// secrets to it.
    SyncingSecrets,
    /// The process is complete.
    Done,
}

/// Named future for granting login by scanning a QR code on this, existing,
/// device that was generated by the other, new, device.
#[derive(Debug)]
pub struct GrantLoginWithScannedQrCode<'a> {
    client: &'a Client,
    qr_code_data: &'a QrCodeData,
    device_creation_timeout: Duration,
    state: SharedObservable<GrantLoginProgress<QrProgress>>,
}

impl<'a> GrantLoginWithScannedQrCode<'a> {
    pub(crate) fn new(
        client: &'a Client,
        qr_code_data: &'a QrCodeData,
        device_creation_timeout: Duration,
    ) -> GrantLoginWithScannedQrCode<'a> {
        GrantLoginWithScannedQrCode {
            client,
            qr_code_data,
            device_creation_timeout,
            state: Default::default(),
        }
    }
}

impl GrantLoginWithScannedQrCode<'_> {
    /// Subscribe to the progress of QR code login.
    ///
    /// It's necessary to subscribe to this to capture the [`CheckCode`] in
    /// order to display it to the other device and to obtain the
    /// verification URL for consenting to the login.
    pub fn subscribe_to_progress(
        &self,
    ) -> impl Stream<Item = GrantLoginProgress<QrProgress>> + use<> {
        self.state.subscribe()
    }
}

impl<'a> IntoFuture for GrantLoginWithScannedQrCode<'a> {
    type Output = Result<(), QRCodeGrantLoginError>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            // Before we get here, the other device has created a new rendezvous session
            // and presented a QR code which this device has scanned.
            // -- MSC4108 Secure channel setup steps 1-3

            // First things first, establish the secure channel. Since we're the one that
            // scanned the QR code, we're certain that the secure channel is
            // secure, under the assumption that we didn't scan the wrong QR code.
            // -- MSC4108 Secure channel setup steps 3-5
            let mut channel = EstablishedSecureChannel::from_qr_code(
                self.client.inner.http_client.inner.clone(),
                self.qr_code_data,
                QrCodeMode::Reciprocate,
            )
            .await?;

            // The other side isn't yet sure that it's talking to the right device, show
            // a check code so they can confirm.
            // -- MSC4108 Secure channel setup step 6
            let check_code = channel.check_code().to_owned();
            self.state
                .set(GrantLoginProgress::EstablishingSecureChannel(QrProgress { check_code }));

            // The user now enters the checkcode on the other device which verifies it
            // and will only continue requesting the login if the code matches.
            // -- MSC4108 Secure channel setup step 7

            // Inform the other device about the available login protocols and the
            // homeserver to use.
            // -- MSC4108 OAuth 2.0 login step 1
            let message = QrAuthMessage::LoginProtocols {
                protocols: vec![LoginProtocolType::DeviceAuthorizationGrant],
                homeserver: self.client.homeserver(),
            };
            channel.send_json(message).await?;

            // Proceed with granting the login.
            // -- MSC4108 OAuth 2.0 login remaining steps
            finish_login_grant(
                self.client,
                &mut channel,
                self.device_creation_timeout,
                &export_secrets_bundle(self.client).await?,
                &self.state,
            )
            .await
        })
    }
}

/// Named future for granting login by generating a QR code on this, existing,
/// device to be scanned by the other, new, device.
#[derive(Debug)]
pub struct GrantLoginWithGeneratedQrCode<'a> {
    client: &'a Client,
    device_creation_timeout: Duration,
    state: SharedObservable<GrantLoginProgress<GeneratedQrProgress>>,
}

impl<'a> GrantLoginWithGeneratedQrCode<'a> {
    pub(crate) fn new(
        client: &'a Client,
        device_creation_timeout: Duration,
    ) -> GrantLoginWithGeneratedQrCode<'a> {
        GrantLoginWithGeneratedQrCode { client, device_creation_timeout, state: Default::default() }
    }
}

impl GrantLoginWithGeneratedQrCode<'_> {
    /// Subscribe to the progress of QR code login.
    ///
    /// It's necessary to subscribe to this to capture the QR code in order to
    /// display it to the other device, to feed the [`CheckCode`] entered by the
    /// user back in and to obtain the verification URL for consenting to
    /// the login.
    pub fn subscribe_to_progress(
        &self,
    ) -> impl Stream<Item = GrantLoginProgress<GeneratedQrProgress>> + use<> {
        self.state.subscribe()
    }
}

impl<'a> IntoFuture for GrantLoginWithGeneratedQrCode<'a> {
    type Output = Result<(), QRCodeGrantLoginError>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            // Create a new ephemeral key pair and a rendezvous session to grant a
            // login with.
            // -- MSC4108 Secure channel setup steps 1 & 2
            let homeserver_url = self.client.homeserver();
            let http_client = self.client.inner.http_client.clone();
            let channel = SecureChannel::reciprocate(http_client, &homeserver_url).await?;

            // Extract the QR code data and emit an update so that the caller can
            // present the QR code for scanning by the new device.
            // -- MSC4108 Secure channel setup step 3
            self.state.set(GrantLoginProgress::EstablishingSecureChannel(
                GeneratedQrProgress::QrReady(channel.qr_code_data().clone()),
            ));

            // Wait for the secure channel to connect. The other device now needs to scan
            // the QR code and send us the LoginInitiateMessage which we respond to
            // with the LoginOkMessage. -- MSC4108 step 4 & 5
            let channel = channel.connect().await?;

            // The other device now needs to verify our message, compute the checkcode and
            // display it. We emit a progress update to let the caller prompt the
            // user to enter the checkcode and feed it back to us.
            // -- MSC4108 Secure channel setup step 6
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.state.set(GrantLoginProgress::EstablishingSecureChannel(
                GeneratedQrProgress::QrScanned(CheckCodeSender::new(tx)),
            ));
            let check_code = rx.await.map_err(|_| SecureChannelError::CannotReceiveCheckCode)?;

            // Use the checkcode to verify that the channel is actually secure.
            // -- MSC4108 Secure channel setup step 7
            let mut channel = channel.confirm(check_code)?;

            // Since the QR code was generated on this existing device, the new device can
            // derive the homeserver to use for logging in from the QR code and we
            // don't need to send the m.login.protocols message.
            // -- MSC4108 OAuth 2.0 login step 1

            // Proceed with granting the login.
            // -- MSC4108 OAuth 2.0 login remaining steps
            finish_login_grant(
                self.client,
                &mut channel,
                self.device_creation_timeout,
                &export_secrets_bundle(self.client).await?,
                &self.state,
            )
            .await
        })
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test {
    use assert_matches2::{assert_let, assert_matches};
    use futures_util::StreamExt;
    use matrix_sdk_base::crypto::types::SecretsBundle;
    use matrix_sdk_common::executor::spawn;
    use matrix_sdk_test::async_test;
    use oauth2::{EndUserVerificationUrl, VerificationUriComplete};
    use ruma::{owned_device_id, owned_user_id};
    use tokio::sync::oneshot;
    use tracing::debug;

    use super::*;
    use crate::{
        authentication::oauth::qrcode::{
            LoginFailureReason, QrAuthMessage,
            messages::{AuthorizationGrant, LoginProtocolType},
            secure_channel::{EstablishedSecureChannel, test::MockedRendezvousServer},
        },
        http_client::HttpClient,
        test_utils::mocks::MatrixMockServer,
    };

    enum BobBehaviour {
        HappyPath,
        UnexpectedMessageInsteadOfLoginProtocol,
        DeviceAlreadyExists,
        DeviceNotCreated,
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_login_with_scanned_qr_code(
        behaviour: BobBehaviour,
        qr_code_rx: oneshot::Receiver<QrCodeData>,
        check_code_tx: oneshot::Sender<u8>,
        server: MatrixMockServer,
        // The rendezvous server is here because it contains MockGuards that are tied to the
        // lifetime of the MatrixMockServer. Otherwise we might attempt to drop the
        // MatrixMockServer before the MockGuards.
        _rendezvous_server: MockedRendezvousServer,
        device_authorization_grant: Option<AuthorizationGrant>,
        secrets_bundle: Option<SecretsBundle>,
    ) {
        // Wait for Alice to produce the qr code.
        let qr_code_data = qr_code_rx.await.expect("Bob should receive the QR code");

        // Use the QR code to establish the secure channel from the new client (Bob).
        let mut bob = EstablishedSecureChannel::from_qr_code(
            reqwest::Client::new(),
            &qr_code_data,
            QrCodeMode::Login,
        )
        .await
        .expect("Bob should be able to connect the secure channel");

        // Let Alice know about the checkcode so she can verify the channel.
        check_code_tx
            .send(bob.check_code().to_digit())
            .expect("Bob should be able to send the checkcode");

        match behaviour {
            BobBehaviour::UnexpectedMessageInsteadOfLoginProtocol => {
                // Send an unexpected message and exit.
                let message = QrAuthMessage::LoginSuccess;
                bob.send_json(message).await.unwrap();
                return;
            }
            BobBehaviour::DeviceAlreadyExists => {
                // Mock the endpoint for querying devices so that Alice thinks the device
                // already exists.
                server.mock_get_device().ok().expect(1..).named("get_device").mount().await;

                // Now send the LoginProtocol message.
                let message = QrAuthMessage::LoginProtocol {
                    protocol: LoginProtocolType::DeviceAuthorizationGrant,
                    device_authorization_grant: device_authorization_grant
                        .expect("Bob needs the device authorization grant"),
                    device_id: "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4".to_owned(),
                };
                bob.send_json(message).await.unwrap();

                // Alice should fail the login with the appropriate reason.
                let message = bob
                    .receive_json()
                    .await
                    .expect("Bob should receive the LoginFailure message from Alice");
                assert_let!(QrAuthMessage::LoginFailure { reason, .. } = message);
                assert_matches!(reason, LoginFailureReason::DeviceAlreadyExists);

                return; // Exit.
            }
            _ => {
                // Send the LoginProtocol message.
                let message = QrAuthMessage::LoginProtocol {
                    protocol: LoginProtocolType::DeviceAuthorizationGrant,
                    device_authorization_grant: device_authorization_grant
                        .expect("Bob needs the device authorization grant"),
                    device_id: "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4".to_owned(),
                };
                bob.send_json(message).await.unwrap();
            }
        }

        // Receive the LoginProtocolAccepted message.
        let message = bob
            .receive_json()
            .await
            .expect("Bob should receive the LoginProtocolAccepted message from Alice");
        assert_let!(QrAuthMessage::LoginProtocolAccepted = message);

        match behaviour {
            BobBehaviour::DeviceNotCreated => {
                // Don't mock the endpoint for querying devices so that Alice cannot verify that
                // we have logged in.

                // Send the LoginSuccess message to claim that we have logged in.
                let message = QrAuthMessage::LoginSuccess;
                bob.send_json(message).await.unwrap();

                // Alice should eventually give up querying our device and fail the login with
                // the appropriate reason.
                let message = bob
                    .receive_json()
                    .await
                    .expect("Bob should receive the LoginFailure message from Alice");
                assert_let!(QrAuthMessage::LoginFailure { reason, .. } = message);
                assert_matches!(reason, LoginFailureReason::DeviceNotFound);

                return; // Exit.
            }
            _ => {
                // Mock the endpoint for querying devices so that Alice thinks we have logged
                // in.
                server.mock_get_device().ok().expect(1..).named("get_device").mount().await;

                // Send the LoginSuccess message.
                let message = QrAuthMessage::LoginSuccess;
                bob.send_json(message).await.unwrap();
            }
        }

        // Receive the LoginSecrets message.
        let message = bob
            .receive_json()
            .await
            .expect("Bob should receive the LoginSecrets message from Alice");
        assert_let!(QrAuthMessage::LoginSecrets(bundle) = message);

        // Verify that we received the correct secrets.
        assert_eq!(
            serde_json::to_value(&secrets_bundle).unwrap(),
            serde_json::to_value(&bundle).unwrap()
        );
    }

    #[allow(clippy::too_many_arguments)]
    async fn request_login_with_generated_qr_code(
        behaviour: BobBehaviour,
        channel: SecureChannel,
        check_code_rx: oneshot::Receiver<u8>,
        server: MatrixMockServer,
        // The rendezvous server is here because it contains MockGuards that are tied to the
        // lifetime of the MatrixMockServer. Otherwise we might attempt to drop the
        // MatrixMockServer before the MockGuards.
        _rendezvous_server: MockedRendezvousServer,
        homeserver: Url,
        device_authorization_grant: Option<AuthorizationGrant>,
        secrets_bundle: Option<SecretsBundle>,
    ) {
        // Wait for Alice to scan the qr code and connect the secure channel.
        let channel =
            channel.connect().await.expect("Bob should be able to connect the secure channel");

        // Wait for Alice to send us the checkcode and use it to verify the channel.
        let check_code = check_code_rx.await.expect("Bob should receive the checkcode");
        let mut bob = channel
            .confirm(check_code)
            .expect("Bob should be able to confirm the channel is secure");

        // Receive the LoginProtocols message.
        let message = bob
            .receive_json()
            .await
            .expect("Bob should receive the LoginProtocolAccepted message from Alice");
        assert_let!(
            QrAuthMessage::LoginProtocols { protocols, homeserver: alice_homeserver } = message
        );
        assert_eq!(protocols, vec![LoginProtocolType::DeviceAuthorizationGrant]);
        assert_eq!(alice_homeserver, homeserver);

        match behaviour {
            BobBehaviour::UnexpectedMessageInsteadOfLoginProtocol => {
                // Send an unexpected message and exit.
                let message = QrAuthMessage::LoginSuccess;
                bob.send_json(message).await.unwrap();
                return;
            }
            BobBehaviour::DeviceAlreadyExists => {
                // Mock the endpoint for querying devices so that Alice thinks the device
                // already exists.
                server.mock_get_device().ok().expect(1..).named("get_device").mount().await;

                // Now send the LoginProtocol message.
                let message = QrAuthMessage::LoginProtocol {
                    protocol: LoginProtocolType::DeviceAuthorizationGrant,
                    device_authorization_grant: device_authorization_grant
                        .expect("Bob needs the device authorization grant"),
                    device_id: "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4".to_owned(),
                };
                bob.send_json(message).await.unwrap();

                // Alice should fail the login with the appropriate reason.
                let message = bob
                    .receive_json()
                    .await
                    .expect("Bob should receive the LoginFailure message from Alice");
                assert_let!(QrAuthMessage::LoginFailure { reason, .. } = message);
                assert_matches!(reason, LoginFailureReason::DeviceAlreadyExists);

                return; // Exit.
            }
            _ => {
                // Send the LoginProtocol message.
                let message = QrAuthMessage::LoginProtocol {
                    protocol: LoginProtocolType::DeviceAuthorizationGrant,
                    device_authorization_grant: device_authorization_grant
                        .expect("Bob needs the device authorization grant"),
                    device_id: "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4".to_owned(),
                };
                bob.send_json(message).await.unwrap();
            }
        }

        // Receive the LoginProtocolAccepted message.
        let message = bob
            .receive_json()
            .await
            .expect("Bob should receive the LoginProtocolAccepted message from Alice");
        assert_let!(QrAuthMessage::LoginProtocolAccepted = message);

        match behaviour {
            BobBehaviour::DeviceNotCreated => {
                // Don't mock the endpoint for querying devices so that Alice cannot verify that
                // we have logged in.

                // Send the LoginSuccess message to claim that we have logged in.
                let message = QrAuthMessage::LoginSuccess;
                bob.send_json(message).await.unwrap();

                // Alice should eventually give up querying our device and fail the login with
                // the appropriate reason.
                let message = bob
                    .receive_json()
                    .await
                    .expect("Bob should receive the LoginFailure message from Alice");
                assert_let!(QrAuthMessage::LoginFailure { reason, .. } = message);
                assert_matches!(reason, LoginFailureReason::DeviceNotFound);

                return; // Exit.
            }
            _ => {
                // Mock the endpoint for querying devices so that Alice thinks we have logged
                // in.
                server.mock_get_device().ok().expect(1..).named("get_device").mount().await;

                // Send the LoginSuccess message.
                let message = QrAuthMessage::LoginSuccess;
                bob.send_json(message).await.unwrap();
            }
        }

        // Receive the LoginSecrets message.
        let message = bob
            .receive_json()
            .await
            .expect("Bob should receive the LoginSecrets message from Alice");
        assert_let!(QrAuthMessage::LoginSecrets(bundle) = message);

        // Verify that we received the correct secrets.
        assert_eq!(
            serde_json::to_value(&secrets_bundle).unwrap(),
            serde_json::to_value(&bundle).unwrap()
        );
    }

    #[async_test]
    async fn test_grant_login_with_generated_qr_code() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::MAX).await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        let device_authorization_grant = AuthorizationGrant {
            verification_uri_complete: Some(VerificationUriComplete::new(
                "https://id.matrix.org/device/abcde".to_owned(),
            )),
            verification_uri: EndUserVerificationUrl::new(
                "https://id.matrix.org/device/abcde?code=ABCDE".to_owned(),
            )
            .unwrap(),
        };

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .generate();
        let secrets_bundle = export_secrets_bundle(&alice)
            .await
            .expect("Alice should be able to export the secrets bundle");
        let (qr_code_tx, qr_code_rx) = oneshot::channel();
        let (checkcode_tx, checkcode_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        let verification_uri_complete =
            device_authorization_grant.clone().verification_uri_complete.unwrap().into_secret();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            let mut qr_code_tx = Some(qr_code_tx);
            let mut checkcode_rx = Some(checkcode_rx);

            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(
                        GeneratedQrProgress::QrReady(qr_code_data),
                    ) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                        qr_code_tx
                            .take()
                            .expect("The QR code should only be forwarded once")
                            .send(qr_code_data.clone())
                            .expect("Alice should be able to forward the QR code");
                    }
                    GrantLoginProgress::EstablishingSecureChannel(
                        GeneratedQrProgress::QrScanned(checkcode_sender),
                    ) => {
                        assert_matches!(
                            state,
                            GrantLoginProgress::EstablishingSecureChannel(
                                GeneratedQrProgress::QrReady(_)
                            )
                        );
                        let checkcode = checkcode_rx
                            .take()
                            .expect("The checkcode should only be forwarded once")
                            .await
                            .expect("Alice should receive the checkcode");
                        checkcode_sender
                            .send(checkcode)
                            .await
                            .expect("Alice should be able to forward the checkcode");
                    }
                    GrantLoginProgress::WaitingForAuth { verification_uri } => {
                        assert_matches!(
                            state,
                            GrantLoginProgress::EstablishingSecureChannel(
                                GeneratedQrProgress::QrScanned(_)
                            )
                        );
                        assert_eq!(verification_uri.as_str(), verification_uri_complete);
                    }
                    GrantLoginProgress::SyncingSecrets => {
                        assert_matches!(state, GrantLoginProgress::WaitingForAuth { .. });
                    }
                    GrantLoginProgress::Done => {
                        assert_matches!(state, GrantLoginProgress::SyncingSecrets);
                        break;
                    }
                }
                state = update;
            }
        });

        // Let Bob request the login and run through the process.
        let bob_task = spawn(async move {
            request_login_with_scanned_qr_code(
                BobBehaviour::HappyPath,
                qr_code_rx,
                checkcode_tx,
                server,
                rendezvous_server,
                Some(device_authorization_grant),
                Some(secrets_bundle),
            )
            .await;
        });

        // Wait for all tasks to finish.
        grant.await.expect("Alice should be able to grant the login");
        updates_task.await.expect("Alice should run through all progress states");
        bob_task.await.expect("Bob's task should finish");
    }

    #[async_test]
    async fn test_grant_login_with_scanned_qr_code() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::MAX).await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        let device_authorization_grant = AuthorizationGrant {
            verification_uri_complete: Some(VerificationUriComplete::new(
                "https://id.matrix.org/device/abcde".to_owned(),
            )),
            verification_uri: EndUserVerificationUrl::new(
                "https://id.matrix.org/device/abcde?code=ABCDE".to_owned(),
            )
            .unwrap(),
        };

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create a secure channel on the new client (Bob) and extract the QR code.
        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let channel = SecureChannel::login(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Bob should be able to create a secure channel.");
        let qr_code_data = channel.qr_code_data().clone();

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future using the QR code.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .scan(&qr_code_data);
        let secrets_bundle = export_secrets_bundle(&alice)
            .await
            .expect("Alice should be able to export the secrets bundle");
        let (checkcode_tx, checkcode_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        let verification_uri_complete =
            device_authorization_grant.clone().verification_uri_complete.unwrap().into_secret();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            let mut checkcode_tx = Some(checkcode_tx);

            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                        checkcode_tx
                            .take()
                            .expect("The checkcode should only be forwarded once")
                            .send(check_code.to_digit())
                            .expect("Alice should be able to forward the checkcode");
                    }
                    GrantLoginProgress::WaitingForAuth { verification_uri } => {
                        assert_matches!(
                            state,
                            GrantLoginProgress::EstablishingSecureChannel(QrProgress { .. })
                        );
                        assert_eq!(verification_uri.as_str(), verification_uri_complete);
                    }
                    GrantLoginProgress::SyncingSecrets => {
                        assert_matches!(state, GrantLoginProgress::WaitingForAuth { .. });
                    }
                    GrantLoginProgress::Done => {
                        assert_matches!(state, GrantLoginProgress::SyncingSecrets);
                        break;
                    }
                }
                state = update;
            }
        });

        // Let Bob request the login and run through the process.
        let bob_task = spawn(async move {
            request_login_with_generated_qr_code(
                BobBehaviour::HappyPath,
                channel,
                checkcode_rx,
                server,
                rendezvous_server,
                alice.homeserver(),
                Some(device_authorization_grant),
                Some(secrets_bundle),
            )
            .await;
        });

        // Wait for all tasks to finish.
        grant.await.expect("Alice should be able to grant the login");
        updates_task.await.expect("Alice should run through all progress states");
        bob_task.await.expect("Bob's task should finish");
    }

    #[async_test]
    async fn test_grant_login_with_scanned_qr_code_with_homeserver_swap() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::MAX).await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        let device_authorization_grant = AuthorizationGrant {
            verification_uri_complete: Some(VerificationUriComplete::new(
                "https://id.matrix.org/device/abcde".to_owned(),
            )),
            verification_uri: EndUserVerificationUrl::new(
                "https://id.matrix.org/device/abcde?code=ABCDE".to_owned(),
            )
            .unwrap(),
        };

        let login_server = MatrixMockServer::new().await;

        login_server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        login_server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        login_server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create a secure channel on the new client (Bob) and extract the QR code.
        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let channel = SecureChannel::login(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Bob should be able to create a secure channel.");
        let qr_code_data = channel.qr_code_data().clone();

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = login_server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future using the QR code.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .scan(&qr_code_data);
        let secrets_bundle = export_secrets_bundle(&alice)
            .await
            .expect("Alice should be able to export the secrets bundle");
        let (checkcode_tx, checkcode_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        let verification_uri_complete =
            device_authorization_grant.clone().verification_uri_complete.unwrap().into_secret();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            let mut checkcode_tx = Some(checkcode_tx);

            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                        checkcode_tx
                            .take()
                            .expect("The checkcode should only be forwarded once")
                            .send(check_code.to_digit())
                            .expect("Alice should be able to forward the checkcode");
                    }
                    GrantLoginProgress::WaitingForAuth { verification_uri } => {
                        assert_matches!(
                            state,
                            GrantLoginProgress::EstablishingSecureChannel(QrProgress { .. })
                        );
                        assert_eq!(verification_uri.as_str(), verification_uri_complete);
                    }
                    GrantLoginProgress::SyncingSecrets => {
                        assert_matches!(state, GrantLoginProgress::WaitingForAuth { .. });
                    }
                    GrantLoginProgress::Done => {
                        assert_matches!(state, GrantLoginProgress::SyncingSecrets);
                        break;
                    }
                }
                state = update;
            }
        });

        // Let Bob request the login and run through the process.
        let bob_task = spawn(async move {
            request_login_with_generated_qr_code(
                BobBehaviour::HappyPath,
                channel,
                checkcode_rx,
                login_server,
                rendezvous_server,
                alice.homeserver(),
                Some(device_authorization_grant),
                Some(secrets_bundle),
            )
            .await;
        });

        // Wait for all tasks to finish.
        grant.await.expect("Alice should be able to grant the login");
        updates_task.await.expect("Alice should run through all progress states");
        bob_task.await.expect("Bob's task should finish");
    }

    #[async_test]
    async fn test_grant_login_with_generated_qr_code_unexpected_message_instead_of_login_protocol()
    {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::MAX).await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .generate();
        let (qr_code_tx, qr_code_rx) = oneshot::channel();
        let (checkcode_tx, checkcode_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            let mut qr_code_tx = Some(qr_code_tx);
            let mut checkcode_rx = Some(checkcode_rx);

            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(
                        GeneratedQrProgress::QrReady(qr_code_data),
                    ) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                        qr_code_tx
                            .take()
                            .expect("The QR code should only be forwarded once")
                            .send(qr_code_data.clone())
                            .expect("Alice should be able to forward the QR code");
                    }
                    GrantLoginProgress::EstablishingSecureChannel(
                        GeneratedQrProgress::QrScanned(checkcode_sender),
                    ) => {
                        assert_matches!(
                            state,
                            GrantLoginProgress::EstablishingSecureChannel(
                                GeneratedQrProgress::QrReady(_)
                            )
                        );
                        let checkcode = checkcode_rx
                            .take()
                            .expect("The checkcode should only be forwarded once")
                            .await
                            .expect("Alice should receive the checkcode");
                        checkcode_sender
                            .send(checkcode)
                            .await
                            .expect("Alice should be able to forward the checkcode");
                        break;
                    }
                    _ => {
                        panic!("Alice should abort the process");
                    }
                }
                state = update;
            }
        });

        // Let Bob request the login and run through the process.
        let bob_task = spawn(async move {
            request_login_with_scanned_qr_code(
                BobBehaviour::UnexpectedMessageInsteadOfLoginProtocol,
                qr_code_rx,
                checkcode_tx,
                server,
                rendezvous_server,
                None,
                None,
            )
            .await;
        });

        // Wait for all tasks to finish / fail.
        grant.await.expect_err("Alice should abort the login");
        updates_task.await.expect("Alice should run through all progress states");
        bob_task.await.expect("Bob's task should finish");
    }

    #[async_test]
    async fn test_grant_login_with_scanned_qr_code_unexpected_message_instead_of_login_protocol() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::MAX).await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create a secure channel on the new client (Bob) and extract the QR code.
        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let channel = SecureChannel::login(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Bob should be able to create a secure channel.");
        let qr_code_data = channel.qr_code_data().clone();

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future using the QR code.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .scan(&qr_code_data);
        let (checkcode_tx, checkcode_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            let mut checkcode_tx = Some(checkcode_tx);

            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                        checkcode_tx
                            .take()
                            .expect("The checkcode should only be forwarded once")
                            .send(check_code.to_digit())
                            .expect("Alice should be able to forward the checkcode");
                        break;
                    }
                    _ => {
                        panic!("Alice should abort the process");
                    }
                }
                state = update;
            }
        });

        // Let Bob request the login and run through the process.
        let bob_task = spawn(async move {
            request_login_with_generated_qr_code(
                BobBehaviour::UnexpectedMessageInsteadOfLoginProtocol,
                channel,
                checkcode_rx,
                server,
                rendezvous_server,
                alice.homeserver(),
                None,
                None,
            )
            .await;
        });

        grant.await.expect_err("Alice should abort the login");

        // Wait for all tasks to finish / fail.
        updates_task.await.expect("Alice should run through all progress states");
        bob_task.await.expect("Bob's task should finish");
    }

    #[async_test]
    async fn test_grant_login_with_generated_qr_code_device_already_exists() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::MAX).await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        let device_authorization_grant = AuthorizationGrant {
            verification_uri_complete: Some(VerificationUriComplete::new(
                "https://id.matrix.org/device/abcde".to_owned(),
            )),
            verification_uri: EndUserVerificationUrl::new(
                "https://id.matrix.org/device/abcde?code=ABCDE".to_owned(),
            )
            .unwrap(),
        };

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .generate();
        let (qr_code_tx, qr_code_rx) = oneshot::channel();
        let (checkcode_tx, checkcode_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            let mut qr_code_tx = Some(qr_code_tx);
            let mut checkcode_rx = Some(checkcode_rx);

            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(
                        GeneratedQrProgress::QrReady(qr_code_data),
                    ) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                        qr_code_tx
                            .take()
                            .expect("The QR code should only be forwarded once")
                            .send(qr_code_data.clone())
                            .expect("Alice should be able to forward the QR code");
                    }
                    GrantLoginProgress::EstablishingSecureChannel(
                        GeneratedQrProgress::QrScanned(checkcode_sender),
                    ) => {
                        assert_matches!(
                            state,
                            GrantLoginProgress::EstablishingSecureChannel(
                                GeneratedQrProgress::QrReady(_)
                            )
                        );
                        let checkcode = checkcode_rx
                            .take()
                            .expect("The checkcode should only be forwarded once")
                            .await
                            .expect("Alice should receive the checkcode");
                        checkcode_sender
                            .send(checkcode)
                            .await
                            .expect("Alice should be able to forward the checkcode");
                    }
                    _ => {
                        panic!("Alice should abort the process");
                    }
                }
                state = update;
            }
        });

        // Let Bob request the login and run through the process.
        let bob_task = spawn(async move {
            request_login_with_scanned_qr_code(
                BobBehaviour::DeviceAlreadyExists,
                qr_code_rx,
                checkcode_tx,
                server,
                rendezvous_server,
                Some(device_authorization_grant),
                None,
            )
            .await;
        });

        // Wait for all tasks to finish.
        grant.await.expect_err("Alice should abort the login");
        updates_task.await.expect("Alice should run through all progress states");
        bob_task.await.expect("Bob's task should finish");
    }

    #[async_test]
    async fn test_grant_login_with_scanned_qr_code_device_already_exists() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::MAX).await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        let device_authorization_grant = AuthorizationGrant {
            verification_uri_complete: Some(VerificationUriComplete::new(
                "https://id.matrix.org/device/abcde".to_owned(),
            )),
            verification_uri: EndUserVerificationUrl::new(
                "https://id.matrix.org/device/abcde?code=ABCDE".to_owned(),
            )
            .unwrap(),
        };

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create a secure channel on the new client (Bob) and extract the QR code.
        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let channel = SecureChannel::login(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Bob should be able to create a secure channel.");
        let qr_code_data = channel.qr_code_data().clone();

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future using the QR code.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .scan(&qr_code_data);
        let (checkcode_tx, checkcode_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            let mut checkcode_tx = Some(checkcode_tx);

            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                        checkcode_tx
                            .take()
                            .expect("The checkcode should only be forwarded once")
                            .send(check_code.to_digit())
                            .expect("Alice should be able to forward the checkcode");
                    }
                    _ => {
                        panic!("Alice should abort the process");
                    }
                }
                state = update;
            }
        });

        // Let Bob request the login and run through the process.
        let bob_task = spawn(async move {
            request_login_with_generated_qr_code(
                BobBehaviour::DeviceAlreadyExists,
                channel,
                checkcode_rx,
                server,
                rendezvous_server,
                alice.homeserver(),
                Some(device_authorization_grant),
                None,
            )
            .await;
        });

        // Wait for all tasks to finish.
        grant.await.expect_err("Alice should abort the login");
        updates_task.await.expect("Alice should run through all progress states");
        bob_task.await.expect("Bob's task should finish");
    }

    #[async_test]
    async fn test_grant_login_with_generated_qr_code_device_not_created() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::MAX).await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        let device_authorization_grant = AuthorizationGrant {
            verification_uri_complete: Some(VerificationUriComplete::new(
                "https://id.matrix.org/device/abcde".to_owned(),
            )),
            verification_uri: EndUserVerificationUrl::new(
                "https://id.matrix.org/device/abcde?code=ABCDE".to_owned(),
            )
            .unwrap(),
        };

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .generate();
        let (qr_code_tx, qr_code_rx) = oneshot::channel();
        let (checkcode_tx, checkcode_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        let verification_uri_complete =
            device_authorization_grant.clone().verification_uri_complete.unwrap().into_secret();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            let mut qr_code_tx = Some(qr_code_tx);
            let mut checkcode_rx = Some(checkcode_rx);

            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(
                        GeneratedQrProgress::QrReady(qr_code_data),
                    ) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                        qr_code_tx
                            .take()
                            .expect("The QR code should only be forwarded once")
                            .send(qr_code_data.clone())
                            .expect("Alice should be able to forward the QR code");
                    }
                    GrantLoginProgress::EstablishingSecureChannel(
                        GeneratedQrProgress::QrScanned(checkcode_sender),
                    ) => {
                        assert_matches!(
                            state,
                            GrantLoginProgress::EstablishingSecureChannel(
                                GeneratedQrProgress::QrReady(_)
                            )
                        );
                        let checkcode = checkcode_rx
                            .take()
                            .expect("The checkcode should only be forwarded once")
                            .await
                            .expect("Alice should receive the checkcode");
                        checkcode_sender
                            .send(checkcode)
                            .await
                            .expect("Alice should be able to forward the checkcode");
                    }
                    GrantLoginProgress::WaitingForAuth { verification_uri } => {
                        assert_matches!(
                            state,
                            GrantLoginProgress::EstablishingSecureChannel(
                                GeneratedQrProgress::QrScanned(_)
                            )
                        );
                        assert_eq!(verification_uri.as_str(), verification_uri_complete);
                    }
                    _ => {
                        panic!("Alice should abort the process");
                    }
                }
                state = update;
            }
        });

        // Let Bob request the login and run through the process.
        let bob_task = spawn(async move {
            request_login_with_scanned_qr_code(
                BobBehaviour::DeviceNotCreated,
                qr_code_rx,
                checkcode_tx,
                server,
                rendezvous_server,
                Some(device_authorization_grant),
                None,
            )
            .await;
        });

        grant.await.expect_err("Alice should abort the login");
        updates_task.await.expect("Alice should run through all progress states");
        bob_task.await.expect("Bob's task should finish");
    }

    #[async_test]
    async fn test_grant_login_with_scanned_qr_code_device_not_created() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::MAX).await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        let device_authorization_grant = AuthorizationGrant {
            verification_uri_complete: Some(VerificationUriComplete::new(
                "https://id.matrix.org/device/abcde".to_owned(),
            )),
            verification_uri: EndUserVerificationUrl::new(
                "https://id.matrix.org/device/abcde?code=ABCDE".to_owned(),
            )
            .unwrap(),
        };

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create a secure channel on the new client (Bob) and extract the QR code.
        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let channel = SecureChannel::login(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Bob should be able to create a secure channel.");
        let qr_code_data = channel.qr_code_data().clone();

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future using the QR code.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .scan(&qr_code_data);
        let (checkcode_tx, checkcode_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        let verification_uri_complete =
            device_authorization_grant.clone().verification_uri_complete.unwrap().into_secret();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            let mut checkcode_tx = Some(checkcode_tx);

            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(QrProgress { check_code }) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                        checkcode_tx
                            .take()
                            .expect("The checkcode should only be forwarded once")
                            .send(check_code.to_digit())
                            .expect("Alice should be able to forward the checkcode");
                    }
                    GrantLoginProgress::WaitingForAuth { verification_uri } => {
                        assert_matches!(
                            state,
                            GrantLoginProgress::EstablishingSecureChannel(QrProgress { .. })
                        );
                        assert_eq!(verification_uri.as_str(), verification_uri_complete);
                    }
                    _ => {
                        panic!("Alice should abort the process");
                    }
                }
                state = update;
            }
        });

        // Let Bob request the login and run through the process.
        let bob_task = spawn(async move {
            request_login_with_generated_qr_code(
                BobBehaviour::DeviceNotCreated,
                channel,
                checkcode_rx,
                server,
                rendezvous_server,
                alice.homeserver(),
                Some(device_authorization_grant),
                None,
            )
            .await;
        });

        grant.await.expect_err("Alice should abort the login");
        updates_task.await.expect("Alice should run through all progress states");
        bob_task.await.expect("Bob's task should finish");
    }

    #[async_test]
    async fn test_grant_login_with_generated_qr_code_session_expired() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::from_secs(2))
                .await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .generate();

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(
                        GeneratedQrProgress::QrReady(_),
                    ) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    _ => {
                        panic!("Alice should abort the process");
                    }
                }
                state = update;
            }
        });

        // Bob does not scan the QR code and the channel is never connected.

        // Wait for the rendezvous session to time out.
        assert_matches!(grant.await, Err(QRCodeGrantLoginError::NotFound));
        updates_task.await.expect("Alice should run through all progress states");
    }

    #[async_test]
    async fn test_grant_login_with_scanned_qr_code_session_expired() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server =
            MockedRendezvousServer::new(server.server(), "abcdEFG12345", Duration::from_secs(2))
                .await;
        debug!("Set up rendezvous server mock at {}", rendezvous_server.rendezvous_url);

        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server
            .mock_upload_cross_signing_keys()
            .ok()
            .expect(1)
            .named("upload_xsigning_keys")
            .mount()
            .await;
        server
            .mock_upload_cross_signing_signatures()
            .ok()
            .expect(1)
            .named("upload_xsigning_signatures")
            .mount()
            .await;

        // Create a secure channel on the new client (Bob) and extract the QR code.
        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let channel = SecureChannel::login(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Bob should be able to create a secure channel.");
        let qr_code_data = channel.qr_code_data().clone();

        // Create the existing client (Alice).
        let user_id = owned_user_id!("@alice:example.org");
        let device_id = owned_device_id!("ALICE_DEVICE");
        let alice = server
            .client_builder_for_crypto_end_to_end(&user_id, &device_id)
            .logged_in_with_oauth()
            .build()
            .await;
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to set up cross signing");

        // Prepare the login granting future using the QR code.
        let oauth = alice.oauth();
        let grant = oauth
            .grant_login_with_qr_code()
            .device_creation_timeout(Duration::from_secs(2))
            .scan(&qr_code_data);

        // Spawn the updates task.
        let mut updates = grant.subscribe_to_progress();
        let mut state = grant.state.get();
        assert_matches!(state.clone(), GrantLoginProgress::Starting);
        let updates_task = spawn(async move {
            while let Some(update) = updates.next().await {
                match &update {
                    GrantLoginProgress::Starting => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    GrantLoginProgress::EstablishingSecureChannel(QrProgress { .. }) => {
                        assert_matches!(state, GrantLoginProgress::Starting);
                    }
                    _ => {
                        panic!("Alice should abort the process");
                    }
                }
                state = update;
            }
        });

        // Bob does not connect the channel.

        // Wait for the rendezvous session to time out.
        assert_matches!(grant.await, Err(QRCodeGrantLoginError::NotFound));
        updates_task.await.expect("Alice should run through all progress states");
    }
}
