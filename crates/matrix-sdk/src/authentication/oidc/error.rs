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
use matrix_sdk_base::deserialized_responses::PrivOwnedStr;
use oauth2::ErrorResponseType;
pub use oauth2::{
    basic::{
        BasicErrorResponse, BasicErrorResponseType, BasicRequestTokenError,
        BasicRevocationErrorResponse,
    },
    ConfigurationError, HttpClientError, RequestTokenError, RevocationErrorResponseType,
    StandardErrorResponse,
};
use ruma::serde::{PartialEqAsRefStr, StringEnum};

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

    /// The device ID was not returned by the homeserver after login.
    #[error("missing device ID in response")]
    MissingDeviceId,

    /// The client is not authenticated while the request requires it.
    #[error("client not authenticated")]
    NotAuthenticated,

    /// An error occurred using the OAuth 2.0 authorization code grant.
    #[error("authorization code grant failed: {0}")]
    AuthorizationCode(#[from] OauthAuthorizationCodeError),

    /// An error occurred interacting with the OAuth 2.0 authorization server
    /// while refreshing the access token.
    #[error("failed to refresh token: {0}")]
    RefreshToken(BasicRequestTokenError<HttpClientError<reqwest::Error>>),

    /// An error occurred revoking an OAuth 2.0 access token.
    #[error("failed to log out: {0}")]
    Logout(#[from] OauthTokenRevocationError),

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

/// All errors that can occur when using the Authorization Code grant with the
/// OAuth 2.0 API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OauthAuthorizationCodeError {
    /// The query of the redirect URI doesn't have the expected format.
    #[error(transparent)]
    RedirectUri(#[from] RedirectUriQueryParseError),

    /// The user cancelled the authorization in the web UI.
    #[error("authorization cancelled by the user")]
    Cancelled,

    /// An error occurred when getting the authorization from the user in the
    /// web UI.
    #[error("authorization failed: {0}")]
    Authorization(StandardErrorResponse<AuthorizationCodeErrorResponseType>),

    /// The state used to complete authorization doesn't match any of the
    /// ongoing authorizations.
    #[error("authorization state value is unexpected")]
    InvalidState,

    /// An error occurred interacting with the OAuth 2.0 authorization server
    /// while exchanging the authorization code for an access token.
    #[error("failed to request token: {0}")]
    RequestToken(BasicRequestTokenError<HttpClientError<reqwest::Error>>),
}

impl From<StandardErrorResponse<AuthorizationCodeErrorResponseType>>
    for OauthAuthorizationCodeError
{
    fn from(value: StandardErrorResponse<AuthorizationCodeErrorResponseType>) -> Self {
        if *value.error() == AuthorizationCodeErrorResponseType::AccessDenied {
            // The user cancelled the login in the web view.
            Self::Cancelled
        } else {
            Self::Authorization(value)
        }
    }
}

/// Error response returned by server after requesting an authorization code.
///
/// The fields in this structure are defined in [Section 4.1.2.1 of RFC 6749].
///
/// [Section 4.1.2.1 of RFC 6749]: https://datatracker.ietf.org/doc/html/rfc6749#section-4.1.2.1
#[derive(Clone, StringEnum, PartialEqAsRefStr, Eq)]
#[ruma_enum(rename_all = "snake_case")]
#[non_exhaustive]
pub enum AuthorizationCodeErrorResponseType {
    /// The request is invalid.
    ///
    /// It is missing a required parameter, includes an invalid parameter value,
    /// includes a parameter more than once, or is otherwise malformed.
    InvalidRequest,

    /// The client is not authorized to request an authorization code using this
    /// method.
    UnauthorizedClient,

    /// The resource owner or authorization server denied the request.
    AccessDenied,

    /// The authorization server does not support obtaining an authorization
    /// code using this method.
    UnsupportedResponseType,

    /// The requested scope is invalid, unknown, or malformed.
    InvalidScope,

    /// The authorization server encountered an unexpected error.
    ServerError,

    /// The authorization server is currently unable to handle the request due
    /// to a temporary overloading or maintenance of the server.
    TemporarilyUnavailable,

    #[doc(hidden)]
    _Custom(PrivOwnedStr),
}

impl ErrorResponseType for AuthorizationCodeErrorResponseType {}

/// All errors that can occur when revoking an OAuth 2.0 token.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OauthTokenRevocationError {
    /// Revocation is not supported by the OAuth 2.0 authorization server.
    #[error("token revocation is not supported")]
    NotSupported,

    /// The revocation endpoint URL is insecure.
    #[error(transparent)]
    Url(ConfigurationError),

    /// An error occurred interacting with the OAuth 2.0 authorization server
    /// while revoking the token.
    #[error("failed to revoke token: {0}")]
    Revoke(RequestTokenError<HttpClientError<reqwest::Error>, BasicRevocationErrorResponse>),
}
