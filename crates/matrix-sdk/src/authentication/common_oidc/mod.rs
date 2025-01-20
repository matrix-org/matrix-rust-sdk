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

//! Common modules useful when using OIDC as the auththentication mechanism.

pub(crate) mod oidc_client;

use as_variant::as_variant;
use openidconnect::core::CoreErrorResponseType;
pub use openidconnect::{
    ConfigurationError, DeviceCodeErrorResponseType, DiscoveryError, HttpClientError,
    RequestTokenError, StandardErrorResponse,
};
use thiserror::Error;

use crate::{oidc, HttpError};

/// Error type describing failures in the interaction between the device
/// attempting to log in and the OIDC provider.
#[derive(Debug, Error)]
pub enum DeviceAuhorizationOidcError {
    /// A generic OIDC error happened while we were attempting to register the
    /// device with the OIDC provider.
    #[error(transparent)]
    Oidc(#[from] oidc::OidcError),

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
