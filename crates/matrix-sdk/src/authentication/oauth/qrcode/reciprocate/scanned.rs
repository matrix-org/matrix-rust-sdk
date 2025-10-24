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

//! The reciprocation flow where the existing device generates a QR code to be
//! scanned by the new device.

use eyeball::SharedObservable;
use futures_core::Stream;
use matrix_sdk_base::crypto::types::{SecretsBundle, qr_login::QrCodeData};
use url::Url;

use super::{
    AlmostEstablishedSecureChannel, Error, SecureChannel, export_secrets_bundle, finish_login_grant,
};
use crate::Client;

/// A handler for reciprocating an existing login to a new device.
pub struct ReciprocateHandler {
    /// The existing client.
    client: Client,
    /// The secrets that will be transmitted to the new device.
    secrets_bundle: SecretsBundle,
    /// The secure channel used to communicate with the new device.
    channel: Channel,
    /// The QR code data to be scanned by the new device.
    qr_code_data: QrCodeData,
    /// The current state of the reciprocation process.
    state: SharedObservable<ReciprocateProgress>,
}

impl std::fmt::Debug for ReciprocateHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReciprocateHandler")
            .field("secrets_bundle", &"SecretsBundle")
            .field("channel", &"Channel")
            .field("qr_code_data", &self.qr_code_data)
            .field("state", &self.state)
            .finish()
    }
}

impl ReciprocateHandler {
    /// Subscribe to the progress of the QR code login.
    pub fn subscribe_to_progress(&self) -> impl Stream<Item = ReciprocateProgress> + use<> {
        self.state.subscribe()
    }

    /// Create a new handler.
    ///
    /// This will create a rendezvous session and an associated QR code that can
    /// be scanned by the new device.
    pub(crate) async fn new(client: Client) -> Result<Self, Error> {
        // Create a new ephemeral key pair and a rendezvous session to reciprocate the
        // login with.
        // -- MSC4108 Secure channel setup steps 1 & 2
        let homeserver_url = client.homeserver();
        let http_client = client.inner.http_client.clone();
        let secure_channel = SecureChannel::reciprocate(http_client, &homeserver_url).await?;

        // Extract the QR code data so that the caller can present the QR code for
        // scanning by the new device. -- MSC4108 Secure channel setup step 3
        let qr_code_data = secure_channel.qr_code_data().clone();

        let channel = Channel::Insecure(secure_channel);
        let secrets_bundle = export_secrets_bundle(&client).await?;

        Ok(Self { client, channel, qr_code_data, secrets_bundle, state: Default::default() })
    }

    /// Get the data for the QR code that the new device needs to scan.
    pub fn qr_code_data(&self) -> &QrCodeData {
        &self.qr_code_data
    }

    /// Wait for the new device to scan the QR code.
    pub async fn wait_for_scan(&mut self) -> Result<(), Error> {
        let Channel::Insecure(channel) = self.channel.take() else {
            return Err(Error::InvalidState);
        };

        // Wait for the secure channel to connect. The other device now needs to scan
        // the QR code and send us the LoginInitiateMessage which we respond to
        // with the LoginOkMessage. -- MSC4108 step 4 & 5
        self.channel = Channel::Almost(channel.connect().await?);

        // The other device now needs to verify our message, compute the checkcode and
        // display it. We emit a progress update to let the caller prompt the
        // user to enter the checkcode and feed it back to us.
        // -- MSC4108 Secure channel setup step 6
        self.state.set(ReciprocateProgress::WaitingForCheckCode);

        Ok(())
    }

    /// Confirm the check code entered by the user and finish the reciprocation.
    pub async fn confirm_check_code_and_finish(&mut self, check_code: u8) -> Result<(), Error> {
        let Channel::Almost(channel) = self.channel.take() else {
            return Err(Error::InvalidState);
        };

        // Use the checkcode to verify that the channel is actually secure.
        // -- MSC4108 Secure channel setup step 7
        let mut channel = channel.confirm(check_code)?;

        // Since the QR code was generated on this existing device, the new device can
        // derive the homeserver to use for logging in from the QR code and we
        // don't need to send the m.login.protocols message.
        // -- MSC4108 OAuth 2.0 login step 1

        // Proceed with the reciprocation.
        // -- MSC4108 OAuth 2.0 login remaining steps
        finish_login_grant(&self.client, &mut channel, &self.secrets_bundle, &self.state).await
    }
}

/// The progress of the reciprocation process.
#[derive(Debug, Default, Clone)]
pub enum ReciprocateProgress {
    /// The QR code has been created and this device is waiting for the new
    /// device to scan it.
    #[default]
    Created,
    /// The QR code has been scanned by the new device and this device is
    /// waiting for the user to put in the check code.
    WaitingForCheckCode,
    /// The secure channel has been confirmed using the checkcode and this
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

impl From<super::ReciprocateProgress> for ReciprocateProgress {
    fn from(progress: super::ReciprocateProgress) -> Self {
        match progress {
            super::ReciprocateProgress::WaitingForAuth { verification_uri } => {
                Self::WaitingForAuth { verification_uri }
            }
            super::ReciprocateProgress::SyncingSecrets => Self::SyncingSecrets,
            super::ReciprocateProgress::Done => Self::Done,
        }
    }
}

#[derive(Default)]
enum Channel {
    /// The QR code login process is in progress.
    #[default]
    InProgress,
    /// The secure channel is in the insecure state.
    Insecure(SecureChannel),
    /// The secure channel is almost established.
    Almost(AlmostEstablishedSecureChannel),
    /// The secure channel could not be established and was left in an
    /// inconsistent state.
    Poisoned,
}

impl Channel {
    /// Takes the value of the channel and leaves it Poisoned
    ///
    /// This is not the same as `std::mem::take(&mut channel)`.
    fn take(&mut self) -> Self {
        std::mem::replace(self, Self::Poisoned)
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test {
    use assert_matches2::{assert_let, assert_matches};
    use futures_util::{StreamExt, join};
    use matrix_sdk_base::crypto::types::qr_login::QrCodeMode;
    use matrix_sdk_common::executor::spawn;
    use matrix_sdk_test::async_test;
    use oauth2::{EndUserVerificationUrl, VerificationUriComplete};
    use ruma::{owned_device_id, owned_user_id};
    use tokio::sync::oneshot;
    use tracing::debug;
    use vodozemac::Curve25519PublicKey;

    use super::*;
    use crate::{
        authentication::oauth::qrcode::{
            LoginFailureReason, QrAuthMessage,
            messages::{AuthorizationGrant, LoginProtocolType},
            secure_channel::{EstablishedSecureChannel, test::MockedRendezvousServer},
        },
        test_utils::mocks::MatrixMockServer,
    };

    enum BobBehaviour {
        HappyPath,
        UnexpectedMessageInsteadOfLoginProtocol,
        DeviceAlreadyExists,
        DeviceNotCreated,
    }

    async fn request_login(
        mut bob: EstablishedSecureChannel,
        behaviour: BobBehaviour,
        check_code_tx: oneshot::Sender<u8>,
        device_authorization_grant: AuthorizationGrant,
        server: MatrixMockServer,
        secrets_bundle: SecretsBundle,
    ) {
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
                    device_authorization_grant,
                    device_id: Curve25519PublicKey::from_base64(
                        "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4",
                    )
                    .unwrap(),
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
                    device_authorization_grant,
                    device_id: Curve25519PublicKey::from_base64(
                        "wjLpTLRqbqBzLs63aYaEv2Boi6cFEbbM/sSRQ2oAKk4",
                    )
                    .unwrap(),
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
    async fn test_grant_qr_login() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server = MockedRendezvousServer::new(server.server(), "abcdEFG12345").await;
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

        // Prepare the reciprocation handler.
        let mut handler = alice
            .oauth()
            .grant_login_with_qr_code()
            .scanned()
            .await
            .expect("Alice should be able to create the grant");
        let qr_code_data = handler.qr_code_data().clone();
        let secrets_bundle = handler.secrets_bundle.clone();
        let (check_code_tx, check_code_rx) = oneshot::channel();

        // Spawn the updates task.
        let mut updates = handler.subscribe_to_progress();
        let mut state = handler.state.get();
        let verification_uri_complete =
            device_authorization_grant.clone().verification_uri_complete.unwrap().into_secret();
        assert_matches!(state.clone(), ReciprocateProgress::Created);
        let updates_task = spawn(async move {
            while let Some(update) = updates.next().await {
                match &update {
                    ReciprocateProgress::Created => {
                        assert_matches!(state, ReciprocateProgress::Created);
                    }
                    ReciprocateProgress::WaitingForCheckCode => {
                        assert_matches!(state, ReciprocateProgress::Created);
                    }
                    ReciprocateProgress::WaitingForAuth { verification_uri } => {
                        assert_matches!(state, ReciprocateProgress::WaitingForCheckCode);
                        assert_eq!(verification_uri.as_str(), verification_uri_complete);
                    }
                    ReciprocateProgress::SyncingSecrets => {
                        assert_matches!(state, ReciprocateProgress::WaitingForAuth { .. });
                    }
                    ReciprocateProgress::Done => {
                        assert_matches!(state, ReciprocateProgress::SyncingSecrets);
                        break;
                    }
                }
                state = update;
            }
        });

        // Spawn the handler task.
        let handler_task = spawn(async move {
            handler.wait_for_scan().await.expect("Alice should be able to connect the channel");

            let check_code = check_code_rx.await.expect("Alice should receive the checkcode");
            handler
                .confirm_check_code_and_finish(check_code)
                .await
                .expect("Alice should be able to confirm the checkcode");
        });

        // Use the QR code to establish the secure channel from the new client (Bob).
        let bob = EstablishedSecureChannel::from_qr_code(
            reqwest::Client::new(),
            &qr_code_data,
            QrCodeMode::Login,
        )
        .await
        .expect("Bob should be able to connect the secure channel");

        // Let Bob request the login and run through the process.
        request_login(
            bob,
            BobBehaviour::HappyPath,
            check_code_tx,
            device_authorization_grant,
            server,
            secrets_bundle,
        )
        .await;

        // Wait for Alice's tasks to finish.
        join!(
            async { updates_task.await.expect("Alice should run through all progress states") },
            async { handler_task.await.expect("Alice should be able to grant the login") },
        );
    }

    #[async_test]
    async fn test_grant_qr_login_failure_unexpected_message_instead_of_login_protocol() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server = MockedRendezvousServer::new(server.server(), "abcdEFG12345").await;
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

        // Prepare the reciprocation handler.
        let mut handler = alice
            .oauth()
            .grant_login_with_qr_code()
            .scanned()
            .await
            .expect("Alice should be able to create the grant");
        let qr_code_data = handler.qr_code_data().clone();
        let secrets_bundle = handler.secrets_bundle.clone();
        let (check_code_tx, check_code_rx) = oneshot::channel();

        // Spawn the handler task.
        let handler_task = spawn(async move {
            handler.wait_for_scan().await.expect("Alice should be able to connect the channel");

            let check_code = check_code_rx.await.expect("Alice should receive the checkcode");
            handler
                .confirm_check_code_and_finish(check_code)
                .await
                .expect("Alice should be able to confirm the checkcode");
        });

        // Use the QR code to establish the secure channel from the new client (Bob).
        let bob = EstablishedSecureChannel::from_qr_code(
            reqwest::Client::new(),
            &qr_code_data,
            QrCodeMode::Login,
        )
        .await
        .expect("Bob should be able to connect the secure channel");

        // Let Bob request the login and run through the process.
        request_login(
            bob,
            BobBehaviour::UnexpectedMessageInsteadOfLoginProtocol,
            check_code_tx,
            device_authorization_grant,
            server,
            secrets_bundle,
        )
        .await;

        // Wait for Alice's task to fail.
        handler_task.await.expect_err("Alice should abort the grant");
    }

    #[async_test]
    async fn test_grant_qr_login_failure_device_already_exists() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server = MockedRendezvousServer::new(server.server(), "abcdEFG12345").await;
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

        // Prepare the reciprocation handler.
        let mut handler = alice
            .oauth()
            .grant_login_with_qr_code()
            .scanned()
            .await
            .expect("Alice should be able to create the grant");
        let qr_code_data = handler.qr_code_data().clone();
        let secrets_bundle = handler.secrets_bundle.clone();
        let (check_code_tx, check_code_rx) = oneshot::channel();

        // Spawn the handler task.
        let handler_task = spawn(async move {
            handler.wait_for_scan().await.expect("Alice should be able to connect the channel");

            let check_code = check_code_rx.await.expect("Alice should receive the checkcode");
            handler
                .confirm_check_code_and_finish(check_code)
                .await
                .expect("Alice should be able to confirm the checkcode");
        });

        // Use the QR code to establish the secure channel from the new client (Bob).
        let bob = EstablishedSecureChannel::from_qr_code(
            reqwest::Client::new(),
            &qr_code_data,
            QrCodeMode::Login,
        )
        .await
        .expect("Bob should be able to connect the secure channel");

        // Let Bob request the login and run through the process.
        request_login(
            bob,
            BobBehaviour::DeviceAlreadyExists,
            check_code_tx,
            device_authorization_grant,
            server,
            secrets_bundle,
        )
        .await;

        // Wait for Alice's task to fail.
        handler_task.await.expect_err("Alice should abort the grant");
    }

    #[async_test]
    async fn test_grant_qr_login_failure_device_not_created() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server = MockedRendezvousServer::new(server.server(), "abcdEFG12345").await;
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

        // Prepare the reciprocation handler.
        let mut handler = alice
            .oauth()
            .grant_login_with_qr_code()
            .scanned()
            .await
            .expect("Alice should be able to create the grant");
        let qr_code_data = handler.qr_code_data().clone();
        let secrets_bundle = handler.secrets_bundle.clone();
        let (check_code_tx, check_code_rx) = oneshot::channel();

        // Spawn the handler task.
        let handler_task = spawn(async move {
            handler.wait_for_scan().await.expect("Alice should be able to connect the channel");

            let check_code = check_code_rx.await.expect("Alice should receive the checkcode");
            handler
                .confirm_check_code_and_finish(check_code)
                .await
                .expect("Alice should be able to confirm the checkcode");
        });

        // Use the QR code to establish the secure channel from the new client (Bob).
        let bob = EstablishedSecureChannel::from_qr_code(
            reqwest::Client::new(),
            &qr_code_data,
            QrCodeMode::Login,
        )
        .await
        .expect("Bob should be able to connect the secure channel");

        // Let Bob request the login and run through the process.
        request_login(
            bob,
            BobBehaviour::DeviceNotCreated,
            check_code_tx,
            device_authorization_grant,
            server,
            secrets_bundle,
        )
        .await;

        // Wait for Alice's task to fail.
        handler_task.await.expect_err("Alice should abort the grant");
    }
}
