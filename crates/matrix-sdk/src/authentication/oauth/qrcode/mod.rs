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

//! Types for the QR code login support defined in [MSC4108](https://github.com/matrix-org/matrix-spec-proposals/pull/4108).
//!
//! Please note, QR code logins are only supported when using OAuth 2.0 as the
//! authentication mechanism, native Matrix authentication does not support it.
//!
//! This currently only implements the case where the new device is scanning the
//! QR code. To log in using a QR code, please take a look at the
//! [`OAuth::login_with_qr_code()`] method.

use std::sync::Arc;

use as_variant::as_variant;
pub use matrix_sdk_base::crypto::types::qr_login::{
    LoginQrCodeDecodeError, QrCodeData, QrCodeMode, QrCodeModeData,
};
use matrix_sdk_base::crypto::{SecretImportError, store::SecretsBundleExportError};
pub use oauth2::{
    ConfigurationError, DeviceCodeErrorResponse, DeviceCodeErrorResponseType, HttpClientError,
    RequestTokenError, StandardErrorResponse,
    basic::{BasicErrorResponse, BasicRequestTokenError},
};
use ruma::api::{client::error::ErrorKind, error::FromHttpResponseError};
use thiserror::Error;
use tokio::sync::Mutex;
use url::Url;
use vodozemac::ecies::CheckCode;
pub use vodozemac::ecies::{Error as EciesError, MessageDecodeError};

mod grant;
mod login;
mod messages;
mod rendezvous_channel;
mod secure_channel;

pub use self::{
    grant::{GrantLoginProgress, GrantLoginWithGeneratedQrCode, GrantLoginWithScannedQrCode},
    login::{LoginProgress, LoginWithGeneratedQrCode, LoginWithQrCode},
    messages::{LoginFailureReason, LoginProtocolType, QrAuthMessage},
};
use super::CrossProcessRefreshLockError;
#[cfg(doc)]
use super::OAuth;
use crate::HttpError;

/// The error type for failures while trying to log in a new device using a QR
/// code.
#[derive(Debug, Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error), uniffi(flat_error))]
pub enum QRCodeLoginError {
    /// An error happened while we were communicating with the OAuth 2.0
    /// authorization server.
    #[error(transparent)]
    OAuth(#[from] DeviceAuthorizationOAuthError),

    /// The other device has signaled to us that the login has failed.
    #[error("The login failed, reason: {reason}")]
    LoginFailure {
        /// The reason, as signaled by the other device, for the login failure.
        reason: LoginFailureReason,
        /// The homeserver that we attempted to log in to.
        homeserver: Option<Url>,
    },

    /// An unexpected message was received from the other device.
    #[error("We have received an unexpected message, expected: {expected}, got {received:?}")]
    UnexpectedMessage {
        /// The message we expected.
        expected: &'static str,
        /// The message we received instead.
        received: QrAuthMessage,
    },

    /// An error happened while exchanging messages with the other device.
    #[error(transparent)]
    SecureChannel(SecureChannelError),

    /// The rendezvous session was not found and might have expired.
    #[error("The rendezvous session was not found and might have expired")]
    NotFound,

    /// The cross-process refresh lock failed to be initialized.
    #[error(transparent)]
    CrossProcessRefreshLock(#[from] CrossProcessRefreshLockError),

    /// An error happened while we were trying to discover our user and device
    /// ID, after we have acquired an access token from the OAuth 2.0
    /// authorization server.
    #[error(transparent)]
    UserIdDiscovery(HttpError),

    /// We failed to set the session tokens after we figured out our device and
    /// user IDs.
    #[error(transparent)]
    SessionTokens(crate::Error),

    /// The device keys failed to be uploaded after we successfully logged in.
    #[error(transparent)]
    DeviceKeyUpload(crate::Error),

    /// The secrets bundle we received from the existing device failed to be
    /// imported.
    #[error(transparent)]
    SecretImport(#[from] SecretImportError),

    /// The other party told us to use a different homeserver but we failed to
    /// reset the server URL.
    #[error(transparent)]
    ServerReset(crate::Error),
}

impl From<SecureChannelError> for QRCodeLoginError {
    fn from(e: SecureChannelError) -> Self {
        match e {
            SecureChannelError::RendezvousChannel(HttpError::Api(ref boxed)) => {
                if let FromHttpResponseError::Server(api_error) = boxed.as_ref()
                    && let Some(ErrorKind::NotFound) =
                        api_error.as_client_api_error().and_then(|e| e.error_kind())
                {
                    return Self::NotFound;
                }
                Self::SecureChannel(e)
            }
            e => Self::SecureChannel(e),
        }
    }
}

/// The error type for failures while trying to grant log in to a new device
/// using a QR code.
#[derive(Debug, Error)]
pub enum QRCodeGrantLoginError {
    /// Secrets backup not set up.
    #[error("Secrets backup not set up")]
    MissingSecretsBackup(Option<SecretsBundleExportError>),

    /// The check code was incorrect.
    #[error("The check code was incorrect")]
    InvalidCheckCode,

    /// The device could not be created.
    #[error("The device could not be created")]
    UnableToCreateDevice,

    /// The rendezvous session was not found and might have expired.
    #[error("The rendezvous session was not found and might have expired")]
    NotFound,

    /// Auth handshake error.
    #[error("Auth handshake error: {0}")]
    Unknown(String),

    /// Unsupported protocol.
    #[error("Unsupported protocol: {0}")]
    UnsupportedProtocol(LoginProtocolType),

    /// The requested device ID is already in use.
    #[error("The requested device ID is already in use")]
    DeviceIDAlreadyInUse,
}

impl From<SecureChannelError> for QRCodeGrantLoginError {
    fn from(e: SecureChannelError) -> Self {
        match e {
            SecureChannelError::RendezvousChannel(HttpError::Api(ref boxed)) => {
                if let FromHttpResponseError::Server(api_error) = boxed.as_ref()
                    && let Some(ErrorKind::NotFound) =
                        api_error.as_client_api_error().and_then(|e| e.error_kind())
                {
                    return Self::NotFound;
                }
                Self::Unknown(e.to_string())
            }
            SecureChannelError::InvalidCheckCode => Self::InvalidCheckCode,
            e => Self::Unknown(e.to_string()),
        }
    }
}

impl From<SecretsBundleExportError> for QRCodeGrantLoginError {
    fn from(e: SecretsBundleExportError) -> Self {
        Self::MissingSecretsBackup(Some(e))
    }
}

/// Error type describing failures in the interaction between the device
/// attempting to log in and the OAuth 2.0 authorization server.
#[derive(Debug, Error)]
pub enum DeviceAuthorizationOAuthError {
    /// A generic OAuth 2.0 error happened while we were attempting to register
    /// the device with the OAuth 2.0 authorization server.
    #[error(transparent)]
    OAuth(#[from] crate::authentication::oauth::OAuthError),

    /// The OAuth 2.0 server doesn't support the device authorization grant.
    #[error("OAuth 2.0 server doesn't support the device authorization grant")]
    NoDeviceAuthorizationEndpoint,

    /// An error happened while we attempted to request a device authorization
    /// from the OAuth 2.0 authorization server.
    #[error(transparent)]
    DeviceAuthorization(#[from] BasicRequestTokenError<HttpClientError<reqwest::Error>>),

    /// An error happened while waiting for the access token to be issued and
    /// sent to us by the OAuth 2.0 authorization server.
    #[error(transparent)]
    RequestToken(
        #[from] RequestTokenError<HttpClientError<reqwest::Error>, DeviceCodeErrorResponse>,
    ),
}

impl DeviceAuthorizationOAuthError {
    /// If the [`DeviceAuthorizationOAuthError`] is of the
    /// [`DeviceCodeErrorResponseType`] error variant, return it.
    pub fn as_request_token_error(&self) -> Option<&DeviceCodeErrorResponseType> {
        let error = as_variant!(self, DeviceAuthorizationOAuthError::RequestToken)?;
        let request_token_error = as_variant!(error, RequestTokenError::ServerResponse)?;

        Some(request_token_error.error())
    }
}

/// Error type for failures in when receiving or sending messages over the
/// secure channel.
#[derive(Debug, Error)]
pub enum SecureChannelError {
    /// A message we received over the secure channel was not a valid UTF-8
    /// encoded string.
    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),

    /// A message has failed to be decrypted.
    #[error(transparent)]
    Ecies(#[from] EciesError),

    /// A received message has failed to be decoded.
    #[error(transparent)]
    MessageDecode(#[from] MessageDecodeError),

    /// A message couldn't be deserialized from JSON.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// The secure channel failed to be established because it received an
    /// unexpected message.
    #[error(
        "The secure channel setup has received an unexpected message, expected: {expected}, got {received}"
    )]
    SecureChannelMessage {
        /// The secure channel message we expected.
        expected: &'static str,
        /// The secure channel message we received instead.
        received: String,
    },

    /// The secure channel could not have been established, the check code was
    /// invalid.
    #[error("The secure channel could not have been established, the check code was invalid")]
    InvalidCheckCode,

    /// An error happened in the underlying rendezvous channel.
    #[error("Error in the rendezvous channel: {0:?}")]
    RendezvousChannel(#[from] HttpError),

    /// Both devices have advertised the same intent in the login attempt, i.e.
    /// both sides claim to be a new device.
    #[error(
        "The secure channel could not have been established, \
         the two devices have the same login intent"
    )]
    InvalidIntent,

    /// The secure channel could not have been established, the check code
    /// cannot be received.
    #[error(
        "The secure channel could not have been established, \
         the check code cannot be received"
    )]
    CannotReceiveCheckCode,
}

/// Metadata to be used with [`LoginProgress::EstablishingSecureChannel`]
/// or [`GrantLoginProgress::EstablishingSecureChannel`] when
/// this device is the one scanning the QR code.
///
/// We have established the secure channel, but we need to let the other
/// side know about the [`CheckCode`] so they can verify that the secure
/// channel is indeed secure.
#[derive(Clone, Debug)]
pub struct QrProgress {
    /// The check code we need to, out of band, send to the other device.
    pub check_code: CheckCode,
}

/// Metadata to be used with [`LoginProgress::EstablishingSecureChannel`] and
/// [`GrantLoginProgress::EstablishingSecureChannel`] when this device is the
/// one generating the QR code.
///
/// We have established the secure channel, but we need to let the
/// other device know about the [`QrCodeData`] so they can connect to the
/// channel and let us know about the checkcode so we can verify that the
/// channel is indeed secure.
#[derive(Clone, Debug)]
pub enum GeneratedQrProgress {
    /// The QR code has been created and this device is waiting for the other
    /// device to scan it.
    QrReady(QrCodeData),
    /// The QR code has been scanned by the other device and this device is
    /// waiting for the user to put in the checkcode displayed on the
    /// other device.
    QrScanned(CheckCodeSender),
}

/// Used to pass back the checkcode entered by the user to verify that the
/// secure channel is indeed secure.
#[derive(Clone, Debug)]
pub struct CheckCodeSender {
    inner: Arc<Mutex<Option<tokio::sync::oneshot::Sender<u8>>>>,
}

impl CheckCodeSender {
    pub(crate) fn new(tx: tokio::sync::oneshot::Sender<u8>) -> Self {
        Self { inner: Arc::new(Mutex::new(Some(tx))) }
    }

    /// Send the checkcode.
    ///
    /// Calling this method more than once will result in an error.
    ///
    /// # Arguments
    ///
    /// * `check_code` - The check code in digits representation.
    pub async fn send(&self, check_code: u8) -> Result<(), CheckCodeSenderError> {
        match self.inner.lock().await.take() {
            Some(tx) => tx.send(check_code).map_err(|_| CheckCodeSenderError::CannotSend),
            None => Err(CheckCodeSenderError::AlreadySent),
        }
    }
}

/// Possible errors when calling [`CheckCodeSender::send`].
#[derive(Debug, thiserror::Error)]
pub enum CheckCodeSenderError {
    /// The check code has already been sent.
    #[error("check code already sent.")]
    AlreadySent,
    /// The check code cannot be sent.
    #[error("check code cannot be sent.")]
    CannotSend,
}
