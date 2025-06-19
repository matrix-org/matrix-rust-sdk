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

use std::future::IntoFuture;

use eyeball::SharedObservable;
use futures_core::Stream;
use matrix_sdk_base::{
    SessionMeta, boxed_into_future,
    crypto::types::qr_login::{QrCodeData, QrCodeMode},
    store::RoomLoadSettings,
};
use oauth2::{DeviceCodeErrorResponseType, StandardDeviceAuthorizationResponse};
use ruma::{
    OwnedDeviceId,
    api::client::discovery::get_authorization_server_metadata::msc2965::AuthorizationServerMetadata,
};
use tracing::trace;
use vodozemac::{Curve25519PublicKey, ecies::CheckCode};

use super::{
    DeviceAuthorizationOAuthError, QRCodeLoginError, SecureChannelError,
    messages::{LoginFailureReason, QrAuthMessage},
    secure_channel::EstablishedSecureChannel,
};
#[cfg(doc)]
use crate::authentication::oauth::OAuth;
use crate::{
    Client,
    authentication::oauth::{ClientRegistrationData, OAuthError},
};

async fn send_unexpected_message_error(
    channel: &mut EstablishedSecureChannel,
) -> Result<(), SecureChannelError> {
    channel
        .send_json(QrAuthMessage::LoginFailure {
            reason: LoginFailureReason::UnexpectedMessageReceived,
            homeserver: None,
        })
        .await
}

/// Type telling us about the progress of the QR code login.
#[derive(Clone, Debug, Default)]
pub enum LoginProgress {
    /// We're just starting up, this is the default and initial state.
    #[default]
    Starting,
    /// We have established the secure channel, but we need to let the other
    /// side know about the [`CheckCode`] so they can verify that the secure
    /// channel is indeed secure.
    EstablishingSecureChannel {
        /// The check code we need to, out of band, send to the other device.
        check_code: CheckCode,
    },
    /// We're waiting for the OAuth 2.0 authorization server to give us the
    /// access token. This will only happen if the other device allows the
    /// OAuth 2.0 authorization server to do so.
    WaitingForToken {
        /// The user code the OAuth 2.0 authorization server has given us, the
        /// OAuth 2.0 authorization server might ask the other device to
        /// enter this code.
        user_code: String,
    },
    /// The login process has completed.
    Done,
}

/// Named future for the [`OAuth::login_with_qr_code()`] method.
#[derive(Debug)]
pub struct LoginWithQrCode<'a> {
    client: &'a Client,
    registration_data: Option<&'a ClientRegistrationData>,
    qr_code_data: &'a QrCodeData,
    state: SharedObservable<LoginProgress>,
}

impl LoginWithQrCode<'_> {
    /// Subscribe to the progress of QR code login.
    ///
    /// It's usually necessary to subscribe to this to let the existing device
    /// know about the [`CheckCode`] which is used to verify that the two
    /// devices are communicating in a secure manner.
    pub fn subscribe_to_progress(&self) -> impl Stream<Item = LoginProgress> {
        self.state.subscribe()
    }
}

impl<'a> IntoFuture for LoginWithQrCode<'a> {
    type Output = Result<(), QRCodeLoginError>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            // First things first, establish the secure channel. Since we're the one that
            // scanned the QR code, we're certain that the secure channel is
            // secure, under the assumption that we didn't scan the wrong QR code.
            let mut channel = self.establish_secure_channel().await?;

            trace!("Established the secure channel.");

            // The other side isn't yet sure that it's talking to the right device, show
            // a check code so they can confirm.
            let check_code = channel.check_code().to_owned();
            self.state.set(LoginProgress::EstablishingSecureChannel { check_code });

            // Register the client with the OAuth 2.0 authorization server.
            trace!("Registering the client with the OAuth 2.0 authorization server.");
            let server_metadata = self.register_client().await?;

            // We want to use the Curve25519 public key for the device ID, so let's generate
            // a new vodozemac `Account` now.
            let account = vodozemac::olm::Account::new();
            let public_key = account.identity_keys().curve25519;
            let device_id = public_key;

            // Let's tell the OAuth 2.0 authorization server that we want to log in using
            // the device authorization grant described in [RFC8628](https://datatracker.ietf.org/doc/html/rfc8628).
            trace!("Requesting device authorization.");
            let auth_grant_response =
                self.request_device_authorization(&server_metadata, device_id).await?;

            // Now we need to inform the other device of the login protocols we picked and
            // the URL they should use to log us in.
            trace!("Letting the existing device know about the device authorization grant.");
            let message = QrAuthMessage::authorization_grant_login_protocol(
                (&auth_grant_response).into(),
                device_id,
            );
            channel.send_json(&message).await?;

            // Let's see if the other device agreed to our proposed protocols.
            match channel.receive_json().await? {
                QrAuthMessage::LoginProtocolAccepted => (),
                QrAuthMessage::LoginFailure { reason, homeserver } => {
                    return Err(QRCodeLoginError::LoginFailure { reason, homeserver });
                }
                message => {
                    send_unexpected_message_error(&mut channel).await?;

                    return Err(QRCodeLoginError::UnexpectedMessage {
                        expected: "m.login.protocol_accepted",
                        received: message,
                    });
                }
            }

            // The OAuth 2.0 authorization server may or may not show this user code to
            // double check that we're talking to the right server. Let us display this, so
            // the other device can double check this as well.
            let user_code = auth_grant_response.user_code();
            self.state
                .set(LoginProgress::WaitingForToken { user_code: user_code.secret().to_owned() });

            // Let's now wait for the access token to be provided to use by the OAuth 2.0
            // authorization server.
            trace!("Waiting for the OAuth 2.0 authorization server to give us the access token.");
            if let Err(e) = self.wait_for_tokens(&server_metadata, &auth_grant_response).await {
                // If we received an error, and it's one of the ones we should report to the
                // other side, do so now.
                if let Some(e) = e.as_request_token_error() {
                    match e {
                        DeviceCodeErrorResponseType::AccessDenied => {
                            channel.send_json(QrAuthMessage::LoginDeclined).await?;
                        }
                        DeviceCodeErrorResponseType::ExpiredToken => {
                            channel
                                .send_json(QrAuthMessage::LoginFailure {
                                    reason: LoginFailureReason::AuthorizationExpired,
                                    homeserver: None,
                                })
                                .await?;
                        }
                        _ => (),
                    }
                }

                return Err(e.into());
            };

            // We only received an access token from the OAuth 2.0 authorization server, we
            // have no clue who we are, so we need to figure out our user ID
            // now. TODO: This snippet is almost the same as the
            // OAuth::finish_login_method(), why is that method even a public
            // method and not called as part of the set session tokens method.
            trace!("Discovering our own user id.");
            let whoami_response =
                self.client.whoami().await.map_err(QRCodeLoginError::UserIdDiscovery)?;
            self.client
                .base_client()
                .activate(
                    SessionMeta {
                        user_id: whoami_response.user_id,
                        device_id: OwnedDeviceId::from(device_id.to_base64()),
                    },
                    RoomLoadSettings::default(),
                    Some(account),
                )
                .await
                .map_err(|error| QRCodeLoginError::SessionTokens(error.into()))?;

            self.client.oauth().enable_cross_process_lock().await?;

            // Tell the existing device that we're logged in.
            trace!("Telling the existing device that we successfully logged in.");
            let message = QrAuthMessage::LoginSuccess;
            channel.send_json(&message).await?;

            // Let's wait for the secrets bundle to be sent to us, otherwise we won't be a
            // fully E2EE enabled device.
            trace!("Waiting for the secrets bundle.");
            let bundle = match channel.receive_json().await? {
                QrAuthMessage::LoginSecrets(bundle) => bundle,
                QrAuthMessage::LoginFailure { reason, homeserver } => {
                    return Err(QRCodeLoginError::LoginFailure { reason, homeserver });
                }
                message => {
                    send_unexpected_message_error(&mut channel).await?;

                    return Err(QRCodeLoginError::UnexpectedMessage {
                        expected: "m.login.secrets",
                        received: message,
                    });
                }
            };

            // Import the secrets bundle, this will allow us to sign the device keys with
            // the master key when we upload them.
            self.client.encryption().import_secrets_bundle(&bundle).await?;

            // Upload the device keys, this will ensure that other devices see us as a fully
            // verified device ass soon as this method returns.
            self.client
                .encryption()
                .ensure_device_keys_upload()
                .await
                .map_err(QRCodeLoginError::DeviceKeyUpload)?;

            // Run and wait for the E2EE initialization tasks, this will ensure that we
            // ourselves see us as verified and the recovery/backup states will
            // be known. If we did receive all the secrets in the secrets
            // bundle, then backups will be enabled after this step as well.
            self.client.encryption().spawn_initialization_task(None);
            self.client.encryption().wait_for_e2ee_initialization_tasks().await;

            trace!("successfully logged in and enabled E2EE.");

            // Tell our listener that we're done.
            self.state.set(LoginProgress::Done);

            // And indeed, we are done with the login.
            Ok(())
        })
    }
}

impl<'a> LoginWithQrCode<'a> {
    pub(crate) fn new(
        client: &'a Client,
        qr_code_data: &'a QrCodeData,
        registration_data: Option<&'a ClientRegistrationData>,
    ) -> LoginWithQrCode<'a> {
        LoginWithQrCode { client, registration_data, qr_code_data, state: Default::default() }
    }

    async fn establish_secure_channel(
        &self,
    ) -> Result<EstablishedSecureChannel, SecureChannelError> {
        let http_client = self.client.inner.http_client.inner.clone();

        let channel = EstablishedSecureChannel::from_qr_code(
            http_client,
            self.qr_code_data,
            QrCodeMode::Login,
        )
        .await?;

        Ok(channel)
    }

    /// Register the client with the OAuth 2.0 authorization server.
    ///
    /// Returns the authorization server metadata.
    async fn register_client(
        &self,
    ) -> Result<AuthorizationServerMetadata, DeviceAuthorizationOAuthError> {
        let oauth = self.client.oauth();
        let server_metadata = oauth.server_metadata().await.map_err(OAuthError::from)?;
        oauth.use_registration_data(&server_metadata, self.registration_data).await?;

        Ok(server_metadata)
    }

    async fn request_device_authorization(
        &self,
        server_metadata: &AuthorizationServerMetadata,
        device_id: Curve25519PublicKey,
    ) -> Result<StandardDeviceAuthorizationResponse, DeviceAuthorizationOAuthError> {
        let oauth = self.client.oauth();
        let response = oauth
            .request_device_authorization(server_metadata, Some(device_id.to_base64().into()))
            .await?;
        Ok(response)
    }

    async fn wait_for_tokens(
        &self,
        server_metadata: &AuthorizationServerMetadata,
        auth_response: &StandardDeviceAuthorizationResponse,
    ) -> Result<(), DeviceAuthorizationOAuthError> {
        let oauth = self.client.oauth();
        oauth.exchange_device_code(server_metadata, auth_response).await?;
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod test {
    use assert_matches2::{assert_let, assert_matches};
    use futures_util::{StreamExt, join};
    use matrix_sdk_base::crypto::types::{SecretsBundle, qr_login::QrCodeModeData};
    use matrix_sdk_common::executor::spawn;
    use matrix_sdk_test::async_test;
    use serde_json::json;

    use super::*;
    use crate::{
        authentication::oauth::qrcode::{
            messages::LoginProtocolType,
            secure_channel::{SecureChannel, test::MockedRendezvousServer},
        },
        config::RequestConfig,
        http_client::HttpClient,
        test_utils::{client::oauth::mock_client_metadata, mocks::MatrixMockServer},
    };

    enum AliceBehaviour {
        HappyPath,
        DeclinedProtocol,
        UnexpectedMessage,
        UnexpectedMessageInsteadOfSecrets,
        RefuseSecrets,
    }

    /// The possible token responses.
    enum TokenResponse {
        Ok,
        AccessDenied,
        ExpiredToken,
    }

    fn secrets_bundle() -> SecretsBundle {
        let json = json!({
            "cross_signing": {
                "master_key": "rTtSv67XGS6k/rg6/yTG/m573cyFTPFRqluFhQY+hSw",
                "self_signing_key": "4jbPt7jh5D2iyM4U+3IDa+WthgJB87IQN1ATdkau+xk",
                "user_signing_key": "YkFKtkjcsTxF6UAzIIG/l6Nog/G2RigCRfWj3cjNWeM",
            },
        });

        serde_json::from_value(json).expect("We should be able to deserialize a secrets bundle")
    }

    /// This is most of the code that is required to be the other side, the
    /// existing device, of the QR login dance.
    ///
    /// TODO: Expose this as a feature user can use.
    async fn grant_login(
        alice: SecureChannel,
        check_code_receiver: tokio::sync::oneshot::Receiver<CheckCode>,
        behavior: AliceBehaviour,
    ) {
        let alice = alice.connect().await.expect("Alice should be able to connect the channel");

        let check_code =
            check_code_receiver.await.expect("We should receive the check code from bob");

        let mut alice = alice
            .confirm(check_code.to_digit())
            .expect("Alice should be able to confirm the secure channel");

        let message = alice
            .receive_json()
            .await
            .expect("Alice should be able to receive the initial message from Bob");

        assert_let!(QrAuthMessage::LoginProtocol { protocol, .. } = message);
        assert_eq!(protocol, LoginProtocolType::DeviceAuthorizationGrant);

        let message = match behavior {
            AliceBehaviour::DeclinedProtocol => QrAuthMessage::LoginFailure {
                reason: LoginFailureReason::UnsupportedProtocol,
                homeserver: None,
            },
            AliceBehaviour::UnexpectedMessage => QrAuthMessage::LoginDeclined,
            _ => QrAuthMessage::LoginProtocolAccepted,
        };

        alice.send_json(message).await.unwrap();

        let message: QrAuthMessage = alice.receive_json().await.unwrap();
        assert_let!(QrAuthMessage::LoginSuccess = message);

        let message = match behavior {
            AliceBehaviour::UnexpectedMessageInsteadOfSecrets => QrAuthMessage::LoginDeclined,
            AliceBehaviour::RefuseSecrets => QrAuthMessage::LoginFailure {
                reason: LoginFailureReason::DeviceNotFound,
                homeserver: None,
            },
            _ => QrAuthMessage::LoginSecrets(secrets_bundle()),
        };

        alice.send_json(message).await.unwrap();
    }

    #[async_test]
    async fn test_qr_login() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server = MockedRendezvousServer::new(server.server(), "abcdEFG12345").await;
        let (sender, receiver) = tokio::sync::oneshot::channel();

        let oauth_server = server.oauth();
        oauth_server.mock_server_metadata().ok().expect(1).named("server_metadata").mount().await;
        oauth_server.mock_registration().ok().expect(1).named("registration").mount().await;
        oauth_server
            .mock_device_authorization()
            .ok()
            .expect(1)
            .named("device_authorization")
            .mount()
            .await;
        oauth_server.mock_token().ok().expect(1).named("token").mount().await;

        server.mock_versions().ok().expect(1..).named("versions").mount().await;
        server.mock_who_am_i().ok().expect(1).named("whoami").mount().await;
        server.mock_upload_keys().ok().expect(1).named("upload_keys").mount().await;
        server.mock_query_keys().ok().expect(1).named("query_keys").mount().await;

        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let alice = SecureChannel::new(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Alice should be able to create a secure channel.");

        assert_let!(QrCodeModeData::Reciprocate { server_name } = &alice.qr_code_data().mode_data);

        let bob = Client::builder()
            .server_name_or_homeserver_url(server_name)
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .expect("We should be able to build the Client object from the URL in the QR code");

        let qr_code = alice.qr_code_data().clone();

        let oauth = bob.oauth();
        let registration_data = mock_client_metadata().into();
        let login_bob = oauth.login_with_qr_code(&qr_code, Some(&registration_data));
        let mut updates = login_bob.subscribe_to_progress();

        let updates_task = spawn(async move {
            let mut sender = Some(sender);

            while let Some(update) = updates.next().await {
                match update {
                    LoginProgress::EstablishingSecureChannel { check_code } => {
                        sender
                            .take()
                            .expect("The establishing secure channel update should be received only once")
                            .send(check_code)
                            .expect("Bob should be able to send the check code to Alice");
                    }
                    LoginProgress::Done => break,
                    _ => (),
                }
            }
        });
        let alice_task =
            spawn(async { grant_login(alice, receiver, AliceBehaviour::HappyPath).await });

        join!(
            async {
                login_bob.await.expect("Bob should be able to login");
            },
            async {
                alice_task.await.expect("Alice should have completed it's task successfully");
            },
            async { updates_task.await.unwrap() }
        );

        assert!(bob.encryption().cross_signing_status().await.unwrap().is_complete());
        let own_identity =
            bob.encryption().get_user_identity(bob.user_id().unwrap()).await.unwrap().unwrap();

        assert!(own_identity.is_verified());
    }

    async fn test_failure(
        token_response: TokenResponse,
        alice_behavior: AliceBehaviour,
    ) -> Result<(), QRCodeLoginError> {
        let server = MatrixMockServer::new().await;
        let rendezvous_server = MockedRendezvousServer::new(server.server(), "abcdEFG12345").await;
        let (sender, receiver) = tokio::sync::oneshot::channel();

        let oauth_server = server.oauth();
        oauth_server.mock_server_metadata().ok().expect(1).named("server_metadata").mount().await;
        oauth_server.mock_registration().ok().expect(1).named("registration").mount().await;
        oauth_server
            .mock_device_authorization()
            .ok()
            .expect(1)
            .named("device_authorization")
            .mount()
            .await;

        let token_mock = oauth_server.mock_token();
        let token_mock = match token_response {
            TokenResponse::Ok => token_mock.ok(),
            TokenResponse::AccessDenied => token_mock.access_denied(),
            TokenResponse::ExpiredToken => token_mock.expired_token(),
        };
        token_mock.named("token").mount().await;

        server.mock_versions().ok().named("versions").mount().await;
        server.mock_who_am_i().ok().named("whoami").mount().await;

        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let alice = SecureChannel::new(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Alice should be able to create a secure channel.");

        assert_let!(QrCodeModeData::Reciprocate { server_name } = &alice.qr_code_data().mode_data);

        let bob = Client::builder()
            .server_name_or_homeserver_url(server_name)
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .expect("We should be able to build the Client object from the URL in the QR code");

        let qr_code = alice.qr_code_data().clone();

        let oauth = bob.oauth();
        let registration_data = mock_client_metadata().into();
        let login_bob = oauth.login_with_qr_code(&qr_code, Some(&registration_data));
        let mut updates = login_bob.subscribe_to_progress();

        let _updates_task = spawn(async move {
            let mut sender = Some(sender);

            while let Some(update) = updates.next().await {
                match update {
                    LoginProgress::EstablishingSecureChannel { check_code } => {
                        sender
                            .take()
                            .expect("The establishing secure channel update should be received only once")
                            .send(check_code)
                            .expect("Bob should be able to send the check code to Alice");
                    }
                    LoginProgress::Done => break,
                    _ => (),
                }
            }
        });
        let _alice_task = spawn(async move { grant_login(alice, receiver, alice_behavior).await });
        login_bob.await
    }

    #[async_test]
    async fn test_qr_login_refused_access_token() {
        let result = test_failure(TokenResponse::AccessDenied, AliceBehaviour::HappyPath).await;

        assert_let!(Err(QRCodeLoginError::OAuth(e)) = result);
        assert_eq!(
            e.as_request_token_error(),
            Some(&DeviceCodeErrorResponseType::AccessDenied),
            "The server should have told us that access has been denied."
        );
    }

    #[async_test]
    async fn test_qr_login_expired_token() {
        let result = test_failure(TokenResponse::ExpiredToken, AliceBehaviour::HappyPath).await;

        assert_let!(Err(QRCodeLoginError::OAuth(e)) = result);
        assert_eq!(
            e.as_request_token_error(),
            Some(&DeviceCodeErrorResponseType::ExpiredToken),
            "The server should have told us that access has been denied."
        );
    }

    #[async_test]
    async fn test_qr_login_declined_protocol() {
        let result = test_failure(TokenResponse::Ok, AliceBehaviour::DeclinedProtocol).await;

        assert_let!(Err(QRCodeLoginError::LoginFailure { reason, .. }) = result);
        assert_eq!(
            reason,
            LoginFailureReason::UnsupportedProtocol,
            "Alice should have told us that the protocol is unsupported."
        );
    }

    #[async_test]
    async fn test_qr_login_unexpected_message() {
        let result = test_failure(TokenResponse::Ok, AliceBehaviour::UnexpectedMessage).await;

        assert_let!(Err(QRCodeLoginError::UnexpectedMessage { expected, .. }) = result);
        assert_eq!(expected, "m.login.protocol_accepted");
    }

    #[async_test]
    async fn test_qr_login_unexpected_message_instead_of_secrets() {
        let result =
            test_failure(TokenResponse::Ok, AliceBehaviour::UnexpectedMessageInsteadOfSecrets)
                .await;

        assert_let!(Err(QRCodeLoginError::UnexpectedMessage { expected, .. }) = result);
        assert_eq!(expected, "m.login.secrets");
    }

    #[async_test]
    async fn test_qr_login_refuse_secrets() {
        let result = test_failure(TokenResponse::Ok, AliceBehaviour::RefuseSecrets).await;

        assert_let!(Err(QRCodeLoginError::LoginFailure { reason, .. }) = result);
        assert_eq!(reason, LoginFailureReason::DeviceNotFound);
    }

    #[async_test]
    async fn test_device_authorization_endpoint_missing() {
        let server = MatrixMockServer::new().await;
        let rendezvous_server = MockedRendezvousServer::new(server.server(), "abcdEFG12345").await;
        let (sender, receiver) = tokio::sync::oneshot::channel();

        let oauth_server = server.oauth();
        oauth_server
            .mock_server_metadata()
            .ok_without_device_authorization()
            .expect(1)
            .named("server_metadata")
            .mount()
            .await;
        oauth_server.mock_registration().ok().expect(1).named("registration").mount().await;

        server.mock_versions().ok().named("versions").mount().await;
        server.mock_who_am_i().ok().named("whoami").mount().await;

        let client = HttpClient::new(reqwest::Client::new(), Default::default());
        let alice = SecureChannel::new(client, &rendezvous_server.homeserver_url)
            .await
            .expect("Alice should be able to create a secure channel.");

        assert_let!(QrCodeModeData::Reciprocate { server_name } = &alice.qr_code_data().mode_data);

        let bob = Client::builder()
            .server_name_or_homeserver_url(server_name)
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .expect("We should be able to build the Client object from the URL in the QR code");

        let qr_code = alice.qr_code_data().clone();

        let oauth = bob.oauth();
        let registration_data = mock_client_metadata().into();
        let login_bob = oauth.login_with_qr_code(&qr_code, Some(&registration_data));
        let mut updates = login_bob.subscribe_to_progress();

        let _updates_task = spawn(async move {
            let mut sender = Some(sender);

            while let Some(update) = updates.next().await {
                match update {
                    LoginProgress::EstablishingSecureChannel { check_code } => {
                        sender
                                .take()
                                .expect("The establishing secure channel update should be received only once")
                                .send(check_code)
                                .expect("Bob should be able to send the check code to Alice");
                    }
                    LoginProgress::Done => break,
                    _ => (),
                }
            }
        });
        let _alice_task =
            spawn(async move { grant_login(alice, receiver, AliceBehaviour::HappyPath).await });
        let error = login_bob.await.unwrap_err();

        assert_matches!(
            error,
            QRCodeLoginError::OAuth(DeviceAuthorizationOAuthError::NoDeviceAuthorizationEndpoint)
        );
    }
}
