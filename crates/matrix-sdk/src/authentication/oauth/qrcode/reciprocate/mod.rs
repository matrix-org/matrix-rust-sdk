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

use std::time::{Duration, Instant};

use eyeball::SharedObservable;
use matrix_sdk_base::crypto::{store::SecretsBundleExportError, types::SecretsBundle};
use oauth2::VerificationUriComplete;
use ruma::api::client::device;
use thiserror::Error;
use url::Url;

use super::{
    LoginProtocolType, QrAuthMessage, SecureChannelError,
    secure_channel::{AlmostEstablishedSecureChannel, EstablishedSecureChannel, SecureChannel},
};
use crate::{Client, authentication::oauth::qrcode::LoginFailureReason};

pub mod scanned;

/// Error type for failures related to reciprocating a login.
#[derive(Debug, Error)]
pub enum Error {
    /// Secrets backup not set up.
    #[error("Secrets backup not set up")]
    MissingSecretsBackup(Option<SecretsBundleExportError>),

    /// The request took too long to complete.
    #[error("The request took too long to complete")]
    Timeout,

    /// The check code was incorrect.
    #[error("The check code was incorrect")]
    InvalidCheckCode,

    /// The device could not be created.
    #[error("The device could not be created")]
    UnableToCreateDevice,

    /// Auth handshake error.
    #[error("Auth handshake error: {0}")]
    Unknown(String),

    /// Unsupported protocol.
    #[error("Unsupported protocol: {0}")]
    UnsupportedProtocol(LoginProtocolType),

    /// The requested device ID is already in use.
    #[error("The requested device ID is already in use")]
    DeviceIDAlreadyInUse,

    /// Invalid state.
    #[error("Invalid state")]
    InvalidState,
}

impl From<SecureChannelError> for Error {
    fn from(e: SecureChannelError) -> Self {
        match e {
            SecureChannelError::InvalidCheckCode => Self::InvalidCheckCode,
            e => Self::Unknown(e.to_string()),
        }
    }
}

impl From<SecretsBundleExportError> for Error {
    fn from(e: SecretsBundleExportError) -> Self {
        Self::MissingSecretsBackup(Some(e))
    }
}

/// The progress of the reciprocation process.
enum ReciprocateProgress {
    /// The secure channel has been confirmed and this device is waiting for the
    /// authorization to complete.
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

async fn export_secrets_bundle(client: &Client) -> Result<SecretsBundle, Error> {
    let secrets_bundle = client
        .olm_machine()
        .await
        .as_ref()
        .ok_or_else(|| Error::MissingSecretsBackup(None))?
        .store()
        .export_secrets_bundle()
        .await?;
    Ok(secrets_bundle)
}

async fn finish_login_grant<P: From<ReciprocateProgress>>(
    client: &Client,
    channel: &mut EstablishedSecureChannel,
    secrets_bundle: &SecretsBundle,
    state: &SharedObservable<P>,
) -> Result<(), Error> {
    // The new device registers with the authorization server and sends it a device
    // authorization authorization request.
    // -- MSC4108 OAuth 2.0 login step 2

    // We wait for the new device to send us the m.login.protocol message with the
    // device authorization grant information. -- MSC4108 OAuth 2.0 login step 3
    let message = channel.receive_json().await?;
    let QrAuthMessage::LoginProtocol { device_authorization_grant, protocol, device_id } = message
    else {
        return Err(Error::Unknown(
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
        return Err(Error::UnsupportedProtocol(protocol));
    }

    // We check that the device ID is still available.
    // -- MSC4108 OAuth 2.0 login step 4 continued
    let request = device::get_device::v3::Request::new(device_id.to_base64().into());
    if client.send(request).await.is_ok() {
        channel
            .send_json(QrAuthMessage::LoginFailure {
                reason: LoginFailureReason::DeviceAlreadyExists,
                homeserver: None,
            })
            .await?;
        return Err(Error::DeviceIDAlreadyInUse);
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
    .map_err(|_| Error::UnableToCreateDevice)?;
    state.set(ReciprocateProgress::WaitingForAuth { verification_uri }.into());

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
        return Err(Error::Unknown(
            "Receiving unexpected message when expecting LoginSuccess".to_owned(),
        ));
    };

    // We check that the new device was created successfully allowing for up to 10
    // seconds of delay. -- MSC4108 Secret sharing and device verification step
    // 1
    let request = device::get_device::v3::Request::new(device_id.to_base64().into());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if client.send(request.clone()).await.is_ok() {
            break;
        }
        // If the deadline hasn't yet passed, give it some time and retry the request.
        if Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        // The deadline has passed. Let's fail the login process.
        channel
            .send_json(QrAuthMessage::LoginFailure {
                reason: LoginFailureReason::DeviceNotFound,
                homeserver: None,
            })
            .await?;
        return Err(Error::DeviceIDAlreadyInUse);
    }

    // We send the new device the secrets bundle.
    // -- MSC4108 Secret sharing and device verification step 2
    state.set(ReciprocateProgress::SyncingSecrets.into());
    let message = QrAuthMessage::LoginSecrets(secrets_bundle.clone());
    channel.send_json(&message).await?;

    // And we're done.
    state.set(ReciprocateProgress::Done.into());

    Ok(())
}
