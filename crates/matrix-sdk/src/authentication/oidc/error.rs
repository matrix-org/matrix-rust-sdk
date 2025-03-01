// Copyright 2025 Kévin Commaille
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

//! Error types used in the [`Oidc`](super::Oidc) API.

pub use mas_oidc_client::error::*;
pub use oauth2::{
    basic::{BasicErrorResponse, BasicErrorResponseType, BasicRequestTokenError},
    HttpClientError, RequestTokenError, StandardErrorResponse,
};

pub use super::cross_process::CrossProcessRefreshLockError;

/// An error when trying to parse the query of a redirect URI.
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum RedirectUriQueryParseError {
    /// There is no query part in the URI.
    #[error("No query in URI")]
    MissingQuery,

    /// Deserialization failed.
    #[error("Query is not using one of the defined formats")]
    UnknownFormat,
}

/// All errors that can occur when using the OpenID Connect API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OidcError {
    /// An error occurred when interacting with the provider.
    #[error(transparent)]
    Oidc(Error),

    /// An error occurred when discovering the authorization server's issuer.
    #[error("authorization server discovery failed: {0}")]
    Discovery(#[from] OauthDiscoveryError),

    /// The OpenID Connect Provider doesn't support dynamic client registration.
    ///
    /// The provider probably offers another way to register clients.
    #[error("no dynamic registration support")]
    NoRegistrationSupport,

    /// The client has not registered while the operation requires it.
    #[error("client not registered")]
    NotRegistered,

    /// The supplied redirect URIs are missing or empty.
    #[error("missing or empty redirect URIs")]
    MissingRedirectUri,

    /// The device ID was not returned by the homeserver after login.
    #[error("missing device ID in response")]
    MissingDeviceId,

    /// The client is not authenticated while the request requires it.
    #[error("client not authenticated")]
    NotAuthenticated,

    /// The state used to complete authorization doesn't match an original
    /// value.
    #[error("the supplied state is unexpected")]
    InvalidState,

    /// The user cancelled authorization in the web view.
    #[error("authorization cancelled")]
    CancelledAuthorization,

    /// The login was completed with an invalid callback.
    #[error("the supplied callback URL is invalid")]
    InvalidCallbackUrl,

    /// An error occurred during authorization.
    #[error("authorization failed")]
    Authorization(super::AuthorizationError),

    /// The device ID is invalid.
    #[error("invalid device ID")]
    InvalidDeviceId,

    /// An error occurred interacting with the OAuth 2.0 authorization server
    /// while refreshing the access token.
    #[error("failed to refresh token: {0}")]
    RefreshToken(BasicRequestTokenError<HttpClientError<reqwest::Error>>),

    /// The OpenID Connect Provider doesn't support token revocation, aka
    /// logging out.
    #[error("no token revocation support")]
    NoRevocationSupport,

    /// An error occurred generating a random value.
    #[error(transparent)]
    Rand(rand::Error),

    /// An error occurred parsing a URL.
    #[error(transparent)]
    Url(url::ParseError),

    /// An error occurred caused by the cross-process locks.
    #[error(transparent)]
    LockError(#[from] CrossProcessRefreshLockError),

    /// An unknown error occurred.
    #[error("unknown error")]
    UnknownError(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl<E> From<E> for OidcError
where
    E: Into<Error>,
{
    fn from(value: E) -> Self {
        Self::Oidc(value.into())
    }
}

/// All errors that can occur when discovering the OAuth 2.0 server metadata.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OauthDiscoveryError {
    /// OAuth 2.0 is not supported by the homeserver.
    #[error("OAuth 2.0 is not supported by the homeserver")]
    NotSupported,

    /// An error occurred when making a request to the homeserver.
    #[error(transparent)]
    Http(#[from] crate::HttpError),

    /// The server metadata is invalid.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// An error occurred when making a request to the OpenID Connect provider.
    #[error(transparent)]
    Oidc(#[from] DiscoveryError),
}

impl OauthDiscoveryError {
    /// Whether this error occurred because OAuth 2.0 is not supported by the
    /// homeserver.
    pub fn is_not_supported(&self) -> bool {
        matches!(self, Self::NotSupported)
    }
}
