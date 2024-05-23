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
//! Please note, QR code logins are only supported when using OIDC as the
//! auththentication mechanism, native Matrix authentication does not support
//! it.
//!
//! This currently only implements the case where the new device is scanning the
//! QR code. To log in using a QR code, please take a look at the
//! [`Oidc::login_with_qr_code()`] method

use as_variant::as_variant;
use matrix_sdk_base::crypto::SecretImportError;
pub use openidconnect::{
    core::CoreErrorResponseType, ConfigurationError, DeviceCodeErrorResponseType, DiscoveryError,
    HttpClientError, RequestTokenError, StandardErrorResponse,
};
use thiserror::Error;
use url::Url;
pub use vodozemac::ecies::{Error as EciesError, MessageDecodeError};

#[cfg(doc)]
use crate::oidc::Oidc;
use crate::{oidc::CrossProcessRefreshLockError, HttpError};

mod login;
mod messages;
mod oidc_client;
mod rendezvous_channel;
mod secure_channel;

pub use matrix_sdk_base::crypto::types::qr_login::{
    LoginQrCodeDecodeError, QrCodeData, QrCodeMode, QrCodeModeData,
};

pub use self::{
    login::{LoginProgress, LoginWithQrCode},
    messages::{LoginFailureReason, LoginProtocolType, QrAuthMessage},
};

/// The error type for failures while trying to log in a new device using a QR
/// code.
#[derive(Debug, Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error), uniffi(flat_error))]
pub enum QRCodeLoginError {
    /// An error happened while we were communicating with the OIDC provider.
    #[error(transparent)]
    Oidc(#[from] DeviceAuhorizationOidcError),

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
    SecureChannel(#[from] SecureChannelError),

    /// The cross-process refresh lock failed to be initialized.
    #[error(transparent)]
    CrossProcessRefreshLock(#[from] CrossProcessRefreshLockError),

    /// An error happened while we were trying to discover our user and device
    /// ID, after we have acquired an access token from the OIDC provider.
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
}

/// Error type describing failures in the interaction between the device
/// attempting to log in and the OIDC provider.
#[derive(Debug, Error)]
pub enum DeviceAuhorizationOidcError {
    /// A generic OIDC error happened while we were attempting to register the
    /// device with the OIDC provider.
    #[error(transparent)]
    Oidc(#[from] crate::oidc::OidcError),

    /// The issuer URL failed to be parsed.
    #[error(transparent)]
    InvalidIssuerUrl(#[from] url::ParseError),

    /// There was an error with our device configuration right before attempting
    /// to wait for the access token to be issued by the OIDC provider.
    #[error(transparent)]
    Configuration(#[from] ConfigurationError),

    /// An error happened while we attempted to discover the authentication
    /// issuer URL.
    #[error(transparent)]
    AuthenticationIssuer(HttpError),

    /// An error happened while we attempted to request a device authorization
    /// from the OIDC provider.
    #[error(transparent)]
    DeviceAuthorization(
        #[from]
        RequestTokenError<
            HttpClientError<reqwest::Error>,
            StandardErrorResponse<CoreErrorResponseType>,
        >,
    ),

    /// An error happened while waiting for the access token to be issued and
    /// sent to us by the OIDC provider.
    #[error(transparent)]
    RequestToken(
        #[from]
        RequestTokenError<
            HttpClientError<reqwest::Error>,
            StandardErrorResponse<DeviceCodeErrorResponseType>,
        >,
    ),

    /// An error happened during the discovery of the OIDC provider metadata.
    #[error(transparent)]
    Discovery(#[from] DiscoveryError<HttpClientError<reqwest::Error>>),
}

impl DeviceAuhorizationOidcError {
    /// If the [`DeviceAuhorizationOidcError`] is of the
    /// [`DeviceCodeErrorResponseType`] error variant, return it.
    pub fn as_request_token_error(&self) -> Option<&DeviceCodeErrorResponseType> {
        let error = as_variant!(self, DeviceAuhorizationOidcError::RequestToken)?;
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
    #[error("The secure channel could not have been established, the two devices have the same login intent")]
    InvalidIntent,
}
