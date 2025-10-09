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

//! Error types used in the [`OAuth`](super::OAuth) API.

use matrix_sdk_base::deserialized_responses::PrivOwnedStr;
use oauth2::ErrorResponseType;
pub use oauth2::{
    ConfigurationError, HttpClientError, RequestTokenError, RevocationErrorResponseType,
    StandardErrorResponse,
    basic::{
        BasicErrorResponse, BasicErrorResponseType, BasicRequestTokenError,
        BasicRevocationErrorResponse,
    },
};
use ruma::{
    api::client::discovery::get_authorization_server_metadata::v1::AuthorizationServerMetadataUrlError,
    serde::StringEnum,
};

#[cfg(feature = "e2e-encryption")]
pub use super::cross_process::CrossProcessRefreshLockError;

/// An error when interacting with the OAuth 2.0 authorization server.
pub type OAuthRequestError<T> =
    RequestTokenError<HttpClientError<reqwest::Error>, StandardErrorResponse<T>>;

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

/// All errors that can occur when using the OAuth 2.0 API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OAuthError {
    /// An error occurred when discovering the authorization server's issuer.
    #[error("authorization server discovery failed: {0}")]
    Discovery(#[from] OAuthDiscoveryError),

    /// An error occurred when registering the client with the authorization
    /// server.
    #[error("client registration failed: {0}")]
    ClientRegistration(#[from] OAuthClientRegistrationError),

    /// The client has not registered while the operation requires it.
    #[error("client not registered")]
    NotRegistered,

    /// The client is not authenticated while the request requires it.
    #[error("client not authenticated")]
    NotAuthenticated,

    /// An error occurred using the OAuth 2.0 authorization code grant.
    #[error("authorization code grant failed: {0}")]
    AuthorizationCode(#[from] OAuthAuthorizationCodeError),

    /// An error occurred interacting with the OAuth 2.0 authorization server
    /// while refreshing the access token.
    #[error("failed to refresh token: {0}")]
    RefreshToken(OAuthRequestError<BasicErrorResponseType>),

    /// An error occurred revoking an OAuth 2.0 access token.
    #[error("failed to log out: {0}")]
    Logout(#[from] OAuthTokenRevocationError),

    /// An error occurred caused by the cross-process locks.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    LockError(#[from] CrossProcessRefreshLockError),

    /// The user logged into a session that is different than the one the client
    /// is already using.
    ///
    /// This only happens if the session was already restored, and the user logs
    /// into a new session that is different than the old one.
    #[error("new logged-in session is different than current client session")]
    SessionMismatch,
}

/// All errors that can occur when discovering the OAuth 2.0 server metadata.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OAuthDiscoveryError {
    /// OAuth 2.0 is not supported by the homeserver.
    #[error("OAuth 2.0 is not supported by the homeserver")]
    NotSupported,

    /// An error occurred when making a request to the homeserver.
    #[error(transparent)]
    Http(#[from] crate::HttpError),

    /// The server metadata is invalid.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// The server metadata URLs are insecure.
    #[error(transparent)]
    Validation(#[from] AuthorizationServerMetadataUrlError),

    /// An error occurred when building the OpenID Connect provider
    /// configuration URL.
    #[error(transparent)]
    Url(#[from] url::ParseError),

    /// An error occurred when making a request to the OpenID Connect provider.
    #[error(transparent)]
    Oidc(#[from] OAuthRequestError<BasicErrorResponseType>),
}

impl OAuthDiscoveryError {
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
pub enum OAuthAuthorizationCodeError {
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
    RequestToken(OAuthRequestError<BasicErrorResponseType>),
}

impl From<StandardErrorResponse<AuthorizationCodeErrorResponseType>>
    for OAuthAuthorizationCodeError
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
#[derive(Clone, StringEnum)]
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
pub enum OAuthTokenRevocationError {
    /// The revocation endpoint URL is insecure.
    #[error(transparent)]
    Url(ConfigurationError),

    /// An error occurred interacting with the OAuth 2.0 authorization server
    /// while revoking the token.
    #[error("failed to revoke token: {0}")]
    Revoke(OAuthRequestError<RevocationErrorResponseType>),
}

/// All errors that can occur when registering a client with an OAuth 2.0
/// authorization server.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum OAuthClientRegistrationError {
    /// The authorization server doesn't support dynamic client registration.
    ///
    /// The server probably offers another way to register clients.
    #[error("dynamic client registration is not supported")]
    NotSupported,

    /// Serialization of the client metadata failed.
    #[error("failed to serialize client metadata: {0}")]
    IntoJson(serde_json::Error),

    /// An error occurred when making a request to the OAuth 2.0 authorization
    /// server.
    #[error(transparent)]
    OAuth(#[from] OAuthRequestError<ClientRegistrationErrorResponseType>),

    /// Deserialization of the registration response failed.
    #[error("failed to deserialize registration response: {0}")]
    FromJson(serde_json::Error),
}

/// Error response returned by server after requesting an authorization code.
///
/// The variant of this enum are defined in [Section 3.2.2 of RFC 7591].
///
/// [Section 3.2.2 of RFC 7591]: https://datatracker.ietf.org/doc/html/rfc7591#section-3.2.2
#[derive(Clone, StringEnum)]
#[ruma_enum(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ClientRegistrationErrorResponseType {
    /// The value of one or more redirection URIs is invalid.
    InvalidRedirectUri,

    /// The value of one of the client metadata fields is invalid and the server
    /// has rejected this request.
    InvalidClientMetadata,

    /// The software statement presented is invalid.
    InvalidSoftwareStatement,

    /// The software statement presented is not approved for use by this
    /// authorization server.
    UnapprovedSoftwareStatement,

    #[doc(hidden)]
    _Custom(PrivOwnedStr),
}

impl ErrorResponseType for ClientRegistrationErrorResponseType {}
