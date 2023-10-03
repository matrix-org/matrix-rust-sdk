// Copyright 2020 The Matrix.org Foundation C.I.C.
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

//! Error conditions.

use std::{io::Error as IoError, sync::Arc};

use as_variant::as_variant;
#[cfg(feature = "qrcode")]
use matrix_sdk_base::crypto::ScanError;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::{
    CryptoStoreError, DecryptorError, KeyExportError, MegolmError, OlmError,
};
use matrix_sdk_base::{Error as SdkBaseError, RoomState, StoreError};
use reqwest::Error as ReqwestError;
use ruma::{
    api::{
        client::{
            error::{ErrorBody, ErrorKind},
            uiaa::{UiaaInfo, UiaaResponse},
        },
        error::{FromHttpResponseError, IntoHttpError},
    },
    events::tag::InvalidUserTagName,
    push::{InsertPushRuleError, RemovePushRuleError},
    IdParseError,
};
use serde_json::Error as JsonError;
use thiserror::Error;
use url::ParseError as UrlParseError;

use crate::store_locks::LockStoreError;

/// Result type of the matrix-sdk.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Result type of a pure HTTP request.
pub type HttpResult<T> = std::result::Result<T, HttpError>;

/// An error response from a Matrix API call, using a client API specific
/// representation if the endpoint is from that.
#[derive(Error, Debug)]
pub enum RumaApiError {
    /// A client API response error.
    #[error(transparent)]
    ClientApi(ruma::api::client::Error),

    /// A user-interactive authentication API error.
    ///
    /// When registering or authenticating, the Matrix server can send a
    /// `UiaaInfo` as the error type, this is a User-Interactive Authentication
    /// API response. This represents an error with information about how to
    /// authenticate the user.
    #[error("User-Interactive Authentication required.")]
    Uiaa(UiaaInfo),

    /// Another API response error.
    #[error(transparent)]
    Other(ruma::api::error::MatrixError),
}

impl RumaApiError {
    /// If `self` is `ClientApi(e)`, returns `Some(e)`.
    ///
    /// Otherwise, returns `None`.
    pub fn as_client_api_error(&self) -> Option<&ruma::api::client::Error> {
        as_variant!(self, Self::ClientApi)
    }
}

/// An HTTP error, representing either a connection error or an error while
/// converting the raw HTTP response into a Matrix response.
#[derive(Error, Debug)]
pub enum HttpError {
    /// An error at the HTTP layer.
    #[error(transparent)]
    Reqwest(#[from] ReqwestError),

    /// Queried endpoint requires authentication but was called on an anonymous
    /// client.
    #[error("the queried endpoint requires authentication but was called before logging in")]
    AuthenticationRequired,

    /// Queried endpoint is not meant for clients.
    #[error("the queried endpoint is not meant for clients")]
    NotClientRequest,

    /// An error converting between ruma_*_api types and Hyper types.
    #[error(transparent)]
    Api(FromHttpResponseError<RumaApiError>),

    /// An error converting between ruma_client_api types and Hyper types.
    #[error(transparent)]
    IntoHttp(#[from] IntoHttpError),

    /// The given request can't be cloned and thus can't be retried.
    #[error("The request cannot be cloned")]
    UnableToCloneRequest,

    /// An error occurred while refreshing the access token.
    #[error(transparent)]
    RefreshToken(#[from] RefreshTokenError),
}

#[rustfmt::skip] // stop rustfmt breaking the `<code>` in docs across multiple lines
impl HttpError {
    /// If `self` is
    /// <code>[Api](Self::Api)([Server](FromHttpResponseError::Server)(e))</code>,
    /// returns `Some(e)`.
    ///
    /// Otherwise, returns `None`.
    pub fn as_ruma_api_error(&self) -> Option<&RumaApiError> {
        as_variant!(self, Self::Api(FromHttpResponseError::Server(e)) => e)
    }

    /// Shorthand for
    /// <code>.[as_ruma_api_error](Self::as_ruma_api_error)().[and_then](Option::and_then)([RumaApiError::as_client_api_error])</code>.
    pub fn as_client_api_error(&self) -> Option<&ruma::api::client::Error> {
        self.as_ruma_api_error().and_then(RumaApiError::as_client_api_error)
    }

    /// If `self` is a server error in the `errcode` + `error` format expected
    /// for client-API endpoints, returns the error kind (`errcode`).
    pub fn client_api_error_kind(&self) -> Option<&ErrorKind> {
        self.as_client_api_error().and_then(|e| {
            as_variant!(&e.body, ErrorBody::Standard { kind, .. } => kind)
        })
    }

    /// Try to destructure the error into an universal interactive auth info.
    ///
    /// Some requests require universal interactive auth, doing such a request
    /// will always fail the first time with a 401 status code, the response
    /// body will contain info how the client can authenticate.
    ///
    /// The request will need to be retried, this time containing additional
    /// authentication data.
    ///
    /// This method is an convenience method to get to the info the server
    /// returned on the first, failed request.
    pub fn as_uiaa_response(&self) -> Option<&UiaaInfo> {
        self.as_ruma_api_error().and_then(as_variant!(RumaApiError::Uiaa))
    }
}

/// Internal representation of errors.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Error doing an HTTP request.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// Queried endpoint requires authentication but was called on an anonymous
    /// client.
    #[error("the queried endpoint requires authentication but was called before logging in")]
    AuthenticationRequired,

    /// This request failed because the local data wasn't sufficient.
    #[error("Local cache doesn't contain all necessary data to perform the action.")]
    InsufficientData,

    /// Attempting to restore a session after the olm-machine has already been
    /// set up fails
    #[cfg(feature = "e2e-encryption")]
    #[error("The olm machine has already been initialized")]
    BadCryptoStoreState,

    /// Attempting to access the olm-machine but it is not yet available.
    #[cfg(feature = "e2e-encryption")]
    #[error("The olm machine isn't yet available")]
    NoOlmMachine,

    /// An error de/serializing type for the `StateStore`
    #[error(transparent)]
    SerdeJson(#[from] JsonError),

    /// An IO error happened.
    #[error(transparent)]
    Io(#[from] IoError),

    /// An error occurred in the crypto store.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    CryptoStoreError(#[from] CryptoStoreError),

    /// An error occurred with a cross-process store lock.
    #[error(transparent)]
    CrossProcessLockError(#[from] LockStoreError),

    /// An error occurred during a E2EE operation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    OlmError(#[from] OlmError),

    /// An error occurred during a E2EE group operation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    MegolmError(#[from] MegolmError),

    /// An error occurred during decryption.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    DecryptorError(#[from] DecryptorError),

    /// An error occurred in the state store.
    #[error(transparent)]
    StateStore(#[from] StoreError),

    /// An error encountered when trying to parse an identifier.
    #[error(transparent)]
    Identifier(#[from] IdParseError),

    /// An error encountered when trying to parse a url.
    #[error(transparent)]
    Url(#[from] UrlParseError),

    /// An error while scanning a QR code.
    #[cfg(feature = "qrcode")]
    #[error(transparent)]
    QrCodeScanError(#[from] ScanError),

    /// An error encountered when trying to parse a user tag name.
    #[error(transparent)]
    UserTagName(#[from] InvalidUserTagName),

    /// An error while processing images.
    #[cfg(feature = "image-proc")]
    #[error(transparent)]
    ImageError(#[from] ImageError),

    /// An error occurred within sliding-sync
    #[cfg(feature = "experimental-sliding-sync")]
    #[error(transparent)]
    SlidingSync(#[from] crate::sliding_sync::Error),

    /// Attempted to call a method on a room that requires the user to have a
    /// specific membership state in the room, but the membership state is
    /// different.
    #[error("wrong room state: {0}")]
    WrongRoomState(WrongRoomState),

    /// The client is in inconsistent state. This happens when we set a room to
    /// a specific type, but then cannot get it in this type.
    #[error("The internal client state is inconsistent.")]
    InconsistentState,

    /// Session callbacks have been set multiple times.
    #[error("session callbacks have been set multiple times")]
    MultipleSessionCallbacks,

    /// An error occurred interacting with the OpenID Connect API.
    #[cfg(feature = "experimental-oidc")]
    #[error(transparent)]
    Oidc(#[from] crate::oidc::OidcError),

    /// A concurrent request to a deduplicated request has failed.
    #[error("a concurrent request failed; see logs for details")]
    ConcurrentRequestFailed,

    /// An other error was raised
    /// this might happen because encryption was enabled on the base-crate
    /// but not here and that raised.
    #[error("unknown error: {0}")]
    UnknownError(Box<dyn std::error::Error + Send + Sync>),
}

#[rustfmt::skip] // stop rustfmt breaking the `<code>` in docs across multiple lines
impl Error {
    /// If `self` is
    /// <code>[Http](Self::Http)([Api](HttpError::Api)([Server](FromHttpResponseError::Server)(e)))</code>,
    /// returns `Some(e)`.
    ///
    /// Otherwise, returns `None`.
    pub fn as_ruma_api_error(&self) -> Option<&RumaApiError> {
        as_variant!(self, Self::Http).and_then(|e| e.as_ruma_api_error())
    }

    /// Shorthand for
    /// <code>.[as_ruma_api_error](Self::as_ruma_api_error)().[and_then](Option::and_then)([RumaApiError::as_client_api_error])</code>.
    pub fn as_client_api_error(&self) -> Option<&ruma::api::client::Error> {
        self.as_ruma_api_error().and_then(RumaApiError::as_client_api_error)
    }

    /// If `self` is a server error in the `errcode` + `error` format expected
    /// for client-API endpoints, returns the error kind (`errcode`).
    pub fn client_api_error_kind(&self) -> Option<&ErrorKind> {
        self.as_client_api_error().and_then(|e| {
            as_variant!(&e.body, ErrorBody::Standard { kind, .. } => kind)
        })
    }

    /// Try to destructure the error into an universal interactive auth info.
    ///
    /// Some requests require universal interactive auth, doing such a request
    /// will always fail the first time with a 401 status code, the response
    /// body will contain info how the client can authenticate.
    ///
    /// The request will need to be retried, this time containing additional
    /// authentication data.
    ///
    /// This method is an convenience method to get to the info the server
    /// returned on the first, failed request.
    pub fn as_uiaa_response(&self) -> Option<&UiaaInfo> {
        self.as_ruma_api_error().and_then(as_variant!(RumaApiError::Uiaa))
    }
}

/// Error for the room key importing functionality.
#[cfg(feature = "e2e-encryption")]
#[derive(Error, Debug)]
// This is allowed because key importing isn't enabled under wasm.
#[allow(dead_code)]
pub enum RoomKeyImportError {
    /// An error de/serializing type for the `StateStore`
    #[error(transparent)]
    SerdeJson(#[from] JsonError),

    /// The crypto store isn't yet open. Logging in is required to open the
    /// crypto store.
    #[error("The crypto store hasn't been yet opened, can't import yet.")]
    StoreClosed,

    /// An IO error happened.
    #[error(transparent)]
    Io(#[from] IoError),

    /// An error occurred in the crypto store.
    #[error(transparent)]
    CryptoStore(#[from] CryptoStoreError),

    /// An error occurred while importing the key export.
    #[error(transparent)]
    Export(#[from] KeyExportError),
}

impl From<FromHttpResponseError<ruma::api::client::Error>> for HttpError {
    fn from(err: FromHttpResponseError<ruma::api::client::Error>) -> Self {
        Self::Api(err.map(RumaApiError::ClientApi))
    }
}

impl From<FromHttpResponseError<UiaaResponse>> for HttpError {
    fn from(err: FromHttpResponseError<UiaaResponse>) -> Self {
        Self::Api(err.map(|e| match e {
            UiaaResponse::AuthResponse(i) => RumaApiError::Uiaa(i),
            UiaaResponse::MatrixError(e) => RumaApiError::ClientApi(e),
        }))
    }
}

impl From<FromHttpResponseError<ruma::api::error::MatrixError>> for HttpError {
    fn from(err: FromHttpResponseError<ruma::api::error::MatrixError>) -> Self {
        Self::Api(err.map(RumaApiError::Other))
    }
}

impl From<SdkBaseError> for Error {
    fn from(e: SdkBaseError) -> Self {
        match e {
            SdkBaseError::StateStore(e) => Self::StateStore(e),
            #[cfg(feature = "e2e-encryption")]
            SdkBaseError::CryptoStore(e) => Self::CryptoStoreError(e),
            #[cfg(feature = "e2e-encryption")]
            SdkBaseError::BadCryptoStoreState => Self::BadCryptoStoreState,
            #[cfg(feature = "e2e-encryption")]
            SdkBaseError::OlmError(e) => Self::OlmError(e),
            #[cfg(feature = "eyre")]
            _ => Self::UnknownError(eyre::eyre!(e).into()),
            #[cfg(all(not(feature = "eyre"), feature = "anyhow"))]
            _ => Self::UnknownError(anyhow::anyhow!(e).into()),
            #[cfg(all(not(feature = "eyre"), not(feature = "anyhow")))]
            _ => {
                let e: Box<dyn std::error::Error + Sync + Send> = format!("{e:?}").into();
                Self::UnknownError(e)
            }
        }
    }
}

impl From<ReqwestError> for Error {
    fn from(e: ReqwestError) -> Self {
        Error::Http(HttpError::Reqwest(e))
    }
}

/// All possible errors that can happen during image processing.
#[cfg(feature = "image-proc")]
#[derive(Error, Debug)]
pub enum ImageError {
    /// Error processing the image data.
    #[error(transparent)]
    Proc(#[from] image::ImageError),

    /// The image format is not supported.
    #[error("the image format is not supported")]
    FormatNotSupported,

    /// The thumbnail size is bigger than the original image.
    #[error("the thumbnail size is bigger than the original image size")]
    ThumbnailBiggerThanOriginal,
}

/// Errors that can happen when refreshing an access token.
///
/// This is usually only returned by [`Client::refresh_access_token()`], unless
/// [handling refresh tokens] is activated for the `Client`.
///
/// [`Client::refresh_access_token()`]: crate::Client::refresh_access_token()
/// [handling refresh tokens]: crate::ClientBuilder::handle_refresh_tokens()
#[derive(Debug, Error, Clone)]
pub enum RefreshTokenError {
    /// Tried to send a refresh token request without a refresh token.
    #[error("missing refresh token")]
    RefreshTokenRequired,

    /// An error occurred interacting with the native Matrix authentication API.
    #[error(transparent)]
    MatrixAuth(Arc<HttpError>),

    /// An error occurred interacting with the OpenID Connect API.
    #[cfg(feature = "experimental-oidc")]
    #[error(transparent)]
    Oidc(#[from] Arc<crate::oidc::OidcError>),
}

/// Errors that can occur when manipulating push notification settings.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum NotificationSettingsError {
    /// Invalid parameter.
    #[error("Invalid parameter `{0}`")]
    InvalidParameter(String),
    /// Unable to add push rule.
    #[error("Unable to add push rule")]
    UnableToAddPushRule,
    /// Unable to remove push rule.
    #[error("Unable to remove push rule")]
    UnableToRemovePushRule,
    /// Unable to update push rule.
    #[error("Unable to update push rule")]
    UnableToUpdatePushRule,
    /// Rule not found
    #[error("Rule not found")]
    RuleNotFound(String),
    /// Unable to save the push rules
    #[error("Unable to save push rules")]
    UnableToSavePushRules,
}

impl From<InsertPushRuleError> for NotificationSettingsError {
    fn from(_: InsertPushRuleError) -> Self {
        Self::UnableToAddPushRule
    }
}

impl From<RemovePushRuleError> for NotificationSettingsError {
    fn from(_: RemovePushRuleError) -> Self {
        Self::UnableToRemovePushRule
    }
}

#[derive(Debug, Error)]
#[error("expected: {expected}, got: {got:?}")]
pub struct WrongRoomState {
    expected: &'static str,
    got: RoomState,
}

impl WrongRoomState {
    pub(crate) fn new(expected: &'static str, got: RoomState) -> Self {
        Self { expected, got }
    }
}
