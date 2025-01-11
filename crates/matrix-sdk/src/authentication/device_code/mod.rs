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

//! Device code authentication for ligging in this device on another device
//! without using QR codes
//!
//! Please note, device code logins are only supported when using OIDC as the
//! auththentication mechanism, native Matrix authentication does not support
//! it.

pub use openidconnect::{
    core::CoreErrorResponseType, ConfigurationError, DeviceCodeErrorResponseType, DiscoveryError,
    HttpClientError, RequestTokenError, StandardErrorResponse,
};
use thiserror::Error;
pub use vodozemac::ecies::{Error as EciesError, MessageDecodeError};

#[cfg(doc)]
use crate::oidc::Oidc;
use crate::{oidc::CrossProcessRefreshLockError, HttpError};

mod login;

pub use matrix_sdk_base::crypto::types::qr_login::{
    LoginQrCodeDecodeError, QrCodeData, QrCodeMode, QrCodeModeData,
};

pub use self::login::LoginWithDeviceCode;
use crate::authentication::common_oidc::DeviceAuhorizationOidcError;

/// The error type for failures while trying to log in a new device using a
/// Device code.
#[derive(Debug, Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error), uniffi(flat_error))]
pub enum DeviceCodeLoginError {
    /// An error happened while we were communicating with the OIDC provider.
    #[error(transparent)]
    Oidc(#[from] DeviceAuhorizationOidcError),

    /// Failed to generate random value for pickling the account data
    #[error(transparent)]
    RandGeneratingError(#[from] rand::Error),

    /// Failed to pickle the account data
    #[error(transparent)]
    AccountPickleError(#[from] vodozemac::LibolmPickleError),
}

/// The error type for failures while trying to finish log in a new device using
/// a Device code.
#[derive(Debug, Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error), uniffi(flat_error))]
pub enum DeviceCodeFinishLoginError {
    /// An error happened while we were communicating with the OIDC provider.
    #[error(transparent)]
    Oidc(#[from] DeviceAuhorizationOidcError),

    /// The login failed so we can't finish it here.
    #[error("Can't finish failed login")]
    LoginFailure {},

    /// An error happened while we were trying to discover our user and device
    /// ID, after we have acquired an access token from the OIDC provider.
    #[error(transparent)]
    UserIdDiscovery(HttpError),

    /// We failed to set the session tokens after we figured out our device and
    /// user IDs.
    #[error(transparent)]
    SessionTokens(crate::Error),

    /// The cross-process refresh lock failed to be initialized.
    #[error(transparent)]
    CrossProcessRefreshLock(#[from] CrossProcessRefreshLockError),

    /// Failed to unpickle the account data
    #[error(transparent)]
    AccountPickleError(#[from] vodozemac::LibolmPickleError),
}
