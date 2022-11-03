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

use std::io::Error as IoError;

use http::StatusCode;
#[cfg(feature = "qrcode")]
use matrix_sdk_base::crypto::ScanError;
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::crypto::{
    CryptoStoreError, DecryptorError, KeyExportError, MegolmError, OlmError,
};
use matrix_sdk_base::{Error as SdkBaseError, StoreError};
use reqwest::Error as ReqwestError;
use ruma::{
    api::{
        client::uiaa::{UiaaInfo, UiaaResponse},
        error::{FromHttpResponseError, IntoHttpError, ServerError},
    },
    events::tag::InvalidUserTagName,
    IdParseError,
};
use serde_json::Error as JsonError;
use thiserror::Error;
use url::ParseError as UrlParseError;

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
    /// `UiaaResponse` as the error type, this is a User-Interactive
    /// Authentication API response. This represents an error with
    /// information about how to authenticate the user.
    #[error(transparent)]
    Uiaa(UiaaResponse),

    /// Another API response error.
    #[error(transparent)]
    Other(ruma::api::error::MatrixError),
}

impl RumaApiError {
    /// If `self` is `ClientApi(e)` or `Uiaa(MatrixError(e))`, returns
    /// `Some(e)`.
    ///
    /// Otherwise, returns `None`.
    pub fn as_client_api_error(&self) -> Option<&ruma::api::client::Error> {
        match self {
            Self::ClientApi(e) | Self::Uiaa(UiaaResponse::MatrixError(e)) => Some(e),
            _ => None,
        }
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

    /// The server returned a status code that should be retried.
    #[error("Server returned an error {0}")]
    Server(StatusCode),

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
    /// <code>[Api](Self::Api)([Server](FromHttpResponseError::Server)([Known](ServerError::Known)(e)))</code>,
    /// returns `Some(e)`.
    ///
    /// Otherwise, returns `None`.
    pub fn as_ruma_api_error(&self) -> Option<&RumaApiError> {
        match self {
            Self::Api(FromHttpResponseError::Server(ServerError::Known(e))) => Some(e),
            _ => None,
        }
    }

    /// Shorthand for
    /// <code>.[as_ruma_api_error](Self::as_ruma_api_error)().[and_then](Option::and_then)([RumaApiError::as_client_api_error])</code>.
    pub fn as_client_api_error(&self) -> Option<&ruma::api::client::Error> {
        self.as_ruma_api_error().and_then(RumaApiError::as_client_api_error)
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
        match self.as_ruma_api_error() {
            Some(RumaApiError::Uiaa(UiaaResponse::AuthResponse(i))) => Some(i),
            _ => None,
        }
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
    #[cfg(feature = "sliding-sync")]
    #[error(transparent)]
    SlidingSync(#[from] crate::sliding_sync::Error),

    /// An other error was raised
    /// this might happen because encryption was enabled on the base-crate
    /// but not here and that raised.
    #[error("unknown error: {0}")]
    UnknownError(Box<dyn std::error::Error + Send + Sync>),
}

#[rustfmt::skip] // stop rustfmt breaking the `<code>` in docs across multiple lines
impl Error {
    /// If `self` is
    /// <code>[Http](Self::Http)([Api](HttpError::Api)([Server](FromHttpResponseError::Server)([Known](ServerError::Known)(e))))</code>,
    /// returns `Some(e)`.
    ///
    /// Otherwise, returns `None`.
    pub fn as_ruma_api_error(&self) -> Option<&RumaApiError> {
        match self {
            Error::Http(e) => e.as_ruma_api_error(),
            _ => None,
        }
    }

    /// Shorthand for
    /// <code>.[as_ruma_api_error](Self::as_ruma_api_error)().[and_then](Option::and_then)([RumaApiError::as_client_api_error])</code>.
    pub fn as_client_api_error(&self) -> Option<&ruma::api::client::Error> {
        self.as_ruma_api_error().and_then(RumaApiError::as_client_api_error)
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
        match self.as_ruma_api_error() {
            Some(RumaApiError::Uiaa(UiaaResponse::AuthResponse(i))) => Some(i),
            _ => None,
        }
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
        Self::Api(err.map(|e| e.map(RumaApiError::ClientApi)))
    }
}

impl From<FromHttpResponseError<UiaaResponse>> for HttpError {
    fn from(err: FromHttpResponseError<UiaaResponse>) -> Self {
        Self::Api(err.map(|e| e.map(RumaApiError::Uiaa)))
    }
}

impl From<FromHttpResponseError<ruma::api::error::MatrixError>> for HttpError {
    fn from(err: FromHttpResponseError<ruma::api::error::MatrixError>) -> Self {
        Self::Api(err.map(|e| e.map(RumaApiError::Other)))
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
    /// The Matrix endpoint returned an error.
    #[error(transparent)]
    ClientApi(#[from] ruma::api::client::Error),

    /// Tried to send a refresh token request without a refresh token.
    #[error("missing refresh token")]
    RefreshTokenRequired,

    /// There was an ongoing refresh token call that failed and the error could
    /// not be forwarded.
    #[error("the access token could not be refreshed")]
    UnableToRefreshToken,
}
