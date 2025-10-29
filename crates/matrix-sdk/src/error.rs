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

use std::{io::Error as IoError, sync::Arc, time::Duration};

use as_variant::as_variant;
use http::StatusCode;
#[cfg(feature = "qrcode")]
use matrix_sdk_base::crypto::ScanError;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::{
    CryptoStoreError, DecryptorError, KeyExportError, MegolmError, OlmError,
};
use matrix_sdk_base::{
    Error as SdkBaseError, QueueWedgeError, RoomState, StoreError,
    event_cache::store::EventCacheStoreError, media::store::MediaStoreError,
};
use reqwest::Error as ReqwestError;
use ruma::{
    IdParseError,
    api::{
        client::{
            error::{ErrorKind, RetryAfter},
            uiaa::{UiaaInfo, UiaaResponse},
        },
        error::{FromHttpResponseError, IntoHttpError},
    },
    events::{room::power_levels::PowerLevelsError, tag::InvalidUserTagName},
    push::{InsertPushRuleError, RemovePushRuleError},
};
use serde_json::Error as JsonError;
use thiserror::Error;
use url::ParseError as UrlParseError;

use crate::{
    authentication::oauth::OAuthError, cross_process_lock::CrossProcessLockError,
    event_cache::EventCacheError, media::MediaError, room::reply::ReplyError,
    sliding_sync::Error as SlidingSyncError,
};

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
    /// Error at the HTTP layer.
    #[error(transparent)]
    Reqwest(#[from] ReqwestError),

    /// API response error (deserialization, or a Matrix-specific error).
    // `Box` its inner value to reduce the enum size.
    #[error(transparent)]
    Api(#[from] Box<FromHttpResponseError<RumaApiError>>),

    /// Error when creating an API request (e.g. serialization of
    /// body/headers/query parameters).
    #[error(transparent)]
    IntoHttp(IntoHttpError),

    /// Error while refreshing the access token.
    #[error(transparent)]
    RefreshToken(RefreshTokenError),
}

#[rustfmt::skip] // stop rustfmt breaking the `<code>` in docs across multiple lines
impl HttpError {
    /// If `self` is
    /// <code>[Api](Self::Api)([Server](FromHttpResponseError::Server)(e))</code>,
    /// returns `Some(e)`.
    ///
    /// Otherwise, returns `None`.
    pub fn as_ruma_api_error(&self) -> Option<&RumaApiError> {
        match self {
            Self::Api(error) => {
                as_variant!(error.as_ref(), FromHttpResponseError::Server)
            },
            _ => None
        }
    }

    /// Shorthand for
    /// <code>.[as_ruma_api_error](Self::as_ruma_api_error)().[and_then](Option::and_then)([RumaApiError::as_client_api_error])</code>.
    pub fn as_client_api_error(&self) -> Option<&ruma::api::client::Error> {
        self.as_ruma_api_error().and_then(RumaApiError::as_client_api_error)
    }
}

// Another impl block that's formatted with rustfmt.
impl HttpError {
    /// If `self` is a server error in the `errcode` + `error` format expected
    /// for client-API endpoints, returns the error kind (`errcode`).
    pub fn client_api_error_kind(&self) -> Option<&ErrorKind> {
        self.as_client_api_error().and_then(ruma::api::client::Error::error_kind)
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

    /// Returns whether an HTTP error response should be qualified as transient
    /// or permanent.
    pub(crate) fn retry_kind(&self) -> RetryKind {
        match self {
            // If it was a plain network error, it's either that we're disconnected from the
            // internet, or that the remote is, so retry a few times.
            HttpError::Reqwest(_) => RetryKind::NetworkFailure,

            HttpError::Api(error) => match error.as_ref() {
                FromHttpResponseError::Server(api_error) => RetryKind::from_api_error(api_error),
                _ => RetryKind::Permanent,
            },
            _ => RetryKind::Permanent,
        }
    }
}

impl From<FromHttpResponseError<RumaApiError>> for HttpError {
    fn from(value: FromHttpResponseError<RumaApiError>) -> Self {
        Self::Api(Box::new(value))
    }
}

/// How should we behave with respect to retry behavior after an [`HttpError`]
/// happened?
pub(crate) enum RetryKind {
    /// The request failed because of an error at the network layer.
    NetworkFailure,

    /// The request failed with a "transient" error, meaning it could be retried
    /// either soon, or after a given amount of time expressed in
    /// `retry_after`.
    Transient {
        // This is used only for attempts to retry, so on non-wasm32 code (in the `native` module).
        #[cfg_attr(target_family = "wasm", allow(dead_code))]
        retry_after: Option<Duration>,
    },

    /// The request failed with a non-transient error, and retrying it would
    /// likely cause the same error again, so it's not worth retrying.
    Permanent,
}

impl RetryKind {
    /// Construct a [`RetryKind`] from a Ruma API error.
    ///
    /// The Ruma API error is for errors which have the standard error response
    /// format defined in the [spec].
    ///
    /// [spec]: https://spec.matrix.org/v1.11/client-server-api/#standard-error-response
    fn from_api_error(api_error: &RumaApiError) -> Self {
        match api_error {
            RumaApiError::ClientApi(client_error) => match client_error.error_kind() {
                Some(ErrorKind::LimitExceeded { retry_after }) => {
                    RetryKind::from_retry_after(retry_after.as_ref())
                }
                Some(ErrorKind::Unrecognized) => RetryKind::Permanent,
                _ => RetryKind::from_status_code(client_error.status_code),
            },
            RumaApiError::Other(e) => RetryKind::from_status_code(e.status_code),
            RumaApiError::Uiaa(_) => RetryKind::Permanent,
        }
    }

    /// Create a [`RetryKind`] if we have found a [`RetryAfter`] defined in an
    /// error.
    ///
    /// This method should be used for errors where the server explicitly tells
    /// us how long we must wait before we retry the request again.
    fn from_retry_after(retry_after: Option<&RetryAfter>) -> Self {
        let retry_after = retry_after
            .and_then(|retry_after| match retry_after {
                RetryAfter::Delay(d) => Some(d),
                RetryAfter::DateTime(_) => None,
            })
            .copied();

        Self::Transient { retry_after }
    }

    /// Construct a [`RetryKind`] from a HTTP [`StatusCode`].
    ///
    /// This should be used if we don't have a more specific Matrix style error
    /// which gives us more information about the nature of the error, i.e.
    /// if we received an error from a reverse proxy while the Matrix
    /// homeserver is down.
    fn from_status_code(status_code: StatusCode) -> Self {
        if status_code.as_u16() == 520 {
            // Cloudflare or some other proxy server sent this, meaning the actual
            // homeserver sent some unknown error back
            RetryKind::Permanent
        } else if status_code == StatusCode::TOO_MANY_REQUESTS || status_code.is_server_error() {
            // If the status code is 429, this is requesting a retry in HTTP, without the
            // custom `errcode`. Treat that as a retriable request with no specified
            // retry_after delay.
            RetryKind::Transient { retry_after: None }
        } else {
            RetryKind::Permanent
        }
    }
}

/// Internal representation of errors.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Error doing an HTTP request.
    #[error(transparent)]
    Http(Box<HttpError>),

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
    CryptoStoreError(Box<CryptoStoreError>),

    /// An error occurred with a cross-process store lock.
    #[error(transparent)]
    CrossProcessLockError(Box<CrossProcessLockError>),

    /// An error occurred during a E2EE operation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    OlmError(Box<OlmError>),

    /// An error occurred during a E2EE group operation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    MegolmError(Box<MegolmError>),

    /// An error occurred during decryption.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    DecryptorError(#[from] DecryptorError),

    /// An error occurred in the state store.
    #[error(transparent)]
    StateStore(Box<StoreError>),

    /// An error occurred in the event cache store.
    #[error(transparent)]
    EventCacheStore(Box<EventCacheStoreError>),

    /// An error occurred in the media store.
    #[error(transparent)]
    MediaStore(Box<MediaStoreError>),

    /// An error encountered when trying to parse an identifier.
    #[error(transparent)]
    Identifier(#[from] IdParseError),

    /// An error encountered when trying to parse a url.
    #[error(transparent)]
    Url(#[from] UrlParseError),

    /// An error while scanning a QR code.
    #[cfg(feature = "qrcode")]
    #[error(transparent)]
    QrCodeScanError(Box<ScanError>),

    /// An error encountered when trying to parse a user tag name.
    #[error(transparent)]
    UserTagName(#[from] InvalidUserTagName),

    /// An error occurred within sliding-sync
    #[error(transparent)]
    SlidingSync(Box<SlidingSyncError>),

    /// Attempted to call a method on a room that requires the user to have a
    /// specific membership state in the room, but the membership state is
    /// different.
    #[error("wrong room state: {0}")]
    WrongRoomState(Box<WrongRoomState>),

    /// Session callbacks have been set multiple times.
    #[error("session callbacks have been set multiple times")]
    MultipleSessionCallbacks,

    /// An error occurred interacting with the OAuth 2.0 API.
    #[error(transparent)]
    OAuth(Box<OAuthError>),

    /// A concurrent request to a deduplicated request has failed.
    #[error("a concurrent request failed; see logs for details")]
    ConcurrentRequestFailed,

    /// An other error was raised.
    ///
    /// This might happen because encryption was enabled on the base-crate
    /// but not here and that raised.
    #[cfg(not(target_family = "wasm"))]
    #[error("unknown error: {0}")]
    UnknownError(Box<dyn std::error::Error + Send + Sync>),

    /// An other error was raised.
    #[cfg(target_family = "wasm")]
    #[error("unknown error: {0}")]
    UnknownError(Box<dyn std::error::Error>),

    /// An error coming from the event cache subsystem.
    #[error(transparent)]
    EventCache(Box<EventCacheError>),

    /// An item has been wedged in the send queue.
    #[error(transparent)]
    SendQueueWedgeError(Box<QueueWedgeError>),

    /// Backups are not enabled
    #[error("backups are not enabled")]
    BackupNotEnabled,

    /// It's forbidden to ignore your own user.
    #[error("can't ignore the logged-in user")]
    CantIgnoreLoggedInUser,

    /// An error happened during handling of a media subrequest.
    #[error(transparent)]
    Media(#[from] MediaError),

    /// An error happened while attempting to reply to an event.
    #[error(transparent)]
    ReplyError(#[from] ReplyError),

    /// An error happened while attempting to change power levels.
    #[error("power levels error: {0}")]
    PowerLevels(#[from] PowerLevelsError),
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
        self.as_client_api_error().and_then(ruma::api::client::Error::error_kind)
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

impl From<HttpError> for Error {
    fn from(error: HttpError) -> Self {
        Error::Http(Box::new(error))
    }
}

#[cfg(feature = "e2e-encryption")]
impl From<CryptoStoreError> for Error {
    fn from(error: CryptoStoreError) -> Self {
        Error::CryptoStoreError(Box::new(error))
    }
}

impl From<CrossProcessLockError> for Error {
    fn from(error: CrossProcessLockError) -> Self {
        Error::CrossProcessLockError(Box::new(error))
    }
}

#[cfg(feature = "e2e-encryption")]
impl From<OlmError> for Error {
    fn from(error: OlmError) -> Self {
        Error::OlmError(Box::new(error))
    }
}

#[cfg(feature = "e2e-encryption")]
impl From<MegolmError> for Error {
    fn from(error: MegolmError) -> Self {
        Error::MegolmError(Box::new(error))
    }
}

impl From<StoreError> for Error {
    fn from(error: StoreError) -> Self {
        Error::StateStore(Box::new(error))
    }
}

impl From<EventCacheStoreError> for Error {
    fn from(error: EventCacheStoreError) -> Self {
        Error::EventCacheStore(Box::new(error))
    }
}

impl From<MediaStoreError> for Error {
    fn from(error: MediaStoreError) -> Self {
        Error::MediaStore(Box::new(error))
    }
}

#[cfg(feature = "qrcode")]
impl From<ScanError> for Error {
    fn from(error: ScanError) -> Self {
        Error::QrCodeScanError(Box::new(error))
    }
}

impl From<SlidingSyncError> for Error {
    fn from(error: SlidingSyncError) -> Self {
        Error::SlidingSync(Box::new(error))
    }
}

impl From<OAuthError> for Error {
    fn from(error: OAuthError) -> Self {
        Error::OAuth(Box::new(error))
    }
}

impl From<EventCacheError> for Error {
    fn from(error: EventCacheError) -> Self {
        Error::EventCache(Box::new(error))
    }
}

impl From<QueueWedgeError> for Error {
    fn from(error: QueueWedgeError) -> Self {
        Error::SendQueueWedgeError(Box::new(error))
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
        Self::Api(Box::new(err.map(RumaApiError::ClientApi)))
    }
}

impl From<FromHttpResponseError<UiaaResponse>> for HttpError {
    fn from(err: FromHttpResponseError<UiaaResponse>) -> Self {
        Self::Api(Box::new(err.map(|e| match e {
            UiaaResponse::AuthResponse(i) => RumaApiError::Uiaa(i),
            UiaaResponse::MatrixError(e) => RumaApiError::ClientApi(e),
        })))
    }
}

impl From<FromHttpResponseError<ruma::api::error::MatrixError>> for HttpError {
    fn from(err: FromHttpResponseError<ruma::api::error::MatrixError>) -> Self {
        Self::Api(Box::new(err.map(RumaApiError::Other)))
    }
}

impl From<SdkBaseError> for Error {
    fn from(e: SdkBaseError) -> Self {
        match e {
            SdkBaseError::StateStore(e) => Self::StateStore(Box::new(e)),
            #[cfg(feature = "e2e-encryption")]
            SdkBaseError::CryptoStore(e) => Self::CryptoStoreError(Box::new(e)),
            #[cfg(feature = "e2e-encryption")]
            SdkBaseError::BadCryptoStoreState => Self::BadCryptoStoreState,
            #[cfg(feature = "e2e-encryption")]
            SdkBaseError::OlmError(e) => Self::OlmError(Box::new(e)),
            #[cfg(feature = "eyre")]
            _ => Self::UnknownError(eyre::eyre!(e).into()),
            #[cfg(all(not(feature = "eyre"), feature = "anyhow", not(target_family = "wasm")))]
            _ => Self::UnknownError(anyhow::anyhow!(e).into()),
            #[cfg(all(not(feature = "eyre"), feature = "anyhow", target_family = "wasm"))]
            _ => Self::UnknownError(e.into()),
            #[cfg(all(
                not(feature = "eyre"),
                not(feature = "anyhow"),
                not(target_family = "wasm")
            ))]
            _ => {
                let e: Box<dyn std::error::Error + Send + Sync> = format!("{e:?}").into();
                Self::UnknownError(e)
            }
            #[cfg(all(not(feature = "eyre"), not(feature = "anyhow"), target_family = "wasm"))]
            _ => {
                let e: Box<dyn std::error::Error> = format!("{e:?}").into();
                Self::UnknownError(e)
            }
        }
    }
}

impl From<ReqwestError> for Error {
    fn from(e: ReqwestError) -> Self {
        Error::Http(Box::new(HttpError::Reqwest(e)))
    }
}

/// Errors that can happen when interacting with the beacon API.
#[derive(Debug, Error)]
pub enum BeaconError {
    // A network error occurred.
    #[error("Network error: {0}")]
    Network(#[from] HttpError),

    // The beacon information is not found.
    #[error("Existing beacon information not found.")]
    NotFound,

    // The redacted event is not an error, but it's not useful for the client.
    #[error("Beacon event is redacted and cannot be processed.")]
    Redacted,

    // The client must join the room to access the beacon information.
    #[error("Must join the room to access beacon information.")]
    Stripped,

    // The beacon event could not be deserialized.
    #[error("Deserialization error: {0}")]
    Deserialization(#[from] serde_json::Error),

    // The beacon event is expired.
    #[error("The beacon event has expired.")]
    NotLive,

    // Allow for other errors to be wrapped.
    #[error("Other error: {0}")]
    Other(Box<Error>),
}

impl From<Error> for BeaconError {
    fn from(err: Error) -> Self {
        BeaconError::Other(Box::new(err))
    }
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

    /// An error occurred interacting with the OAuth 2.0 API.
    #[error(transparent)]
    OAuth(#[from] Arc<OAuthError>),
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
    #[error("Rule `{0}` not found")]
    RuleNotFound(String),
    /// Unable to save the push rules
    #[error("Unable to save push rules")]
    UnableToSavePushRules,
}

impl NotificationSettingsError {
    /// Whether this error is the [`RuleNotFound`](Self::RuleNotFound) variant.
    pub fn is_rule_not_found(&self) -> bool {
        matches!(self, Self::RuleNotFound(_))
    }
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
