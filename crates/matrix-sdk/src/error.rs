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
#[cfg(feature = "encryption")]
use matrix_sdk_base::crypto::{
    CryptoStoreError, DecryptorError, KeyExportError, MegolmError, OlmError,
};
use matrix_sdk_base::{Error as SdkBaseError, StoreError};
use reqwest::Error as ReqwestError;
use ruma::{
    api::{
        client::{
            r0::uiaa::{UiaaInfo, UiaaResponse as UiaaError},
            Error as RumaClientApiError,
        },
        error::{FromHttpResponseError, IntoHttpError, MatrixError as RumaApiError, ServerError},
    },
    identifiers::Error as IdentifierError,
};
use serde_json::Error as JsonError;
use thiserror::Error;
use url::ParseError as UrlParseError;

/// Result type of the matrix-sdk.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Result type of a pure HTTP request.
pub type HttpResult<T> = std::result::Result<T, HttpError>;

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
    Api(#[from] FromHttpResponseError<RumaApiError>),

    /// An error converting between ruma_client_api types and Hyper types.
    #[error(transparent)]
    ClientApi(#[from] FromHttpResponseError<RumaClientApiError>),

    /// An error converting between ruma_client_api types and Hyper types.
    #[error(transparent)]
    IntoHttp(#[from] IntoHttpError),

    /// An error occurred while authenticating.
    ///
    /// When registering or authenticating the Matrix server can send a
    /// `UiaaResponse` as the error type, this is a User-Interactive
    /// Authentication API response. This represents an error with
    /// information about how to authenticate the user.
    #[error(transparent)]
    UiaaError(#[from] FromHttpResponseError<UiaaError>),

    /// The server returned a status code that should be retried.
    #[error("Server returned an error {0}")]
    Server(StatusCode),

    /// The given request can't be cloned and thus can't be retried.
    #[error("The request cannot be cloned")]
    UnableToCloneRequest,

    /// Tried to send a request without `user_id` in the `Session`
    #[error("missing user_id in session")]
    UserIdRequired,
}

/// Internal representation of errors.
#[derive(Error, Debug)]
pub enum Error {
    /// Error doing an HTTP request.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// Queried endpoint requires authentication but was called on an anonymous
    /// client.
    #[error("the queried endpoint requires authentication but was called before logging in")]
    AuthenticationRequired,

    /// An error de/serializing type for the `StateStore`
    #[error(transparent)]
    SerdeJson(#[from] JsonError),

    /// An IO error happened.
    #[error(transparent)]
    Io(#[from] IoError),

    /// An error occurred in the crypto store.
    #[cfg(feature = "encryption")]
    #[error(transparent)]
    CryptoStoreError(#[from] CryptoStoreError),

    /// An error occurred during a E2EE operation.
    #[cfg(feature = "encryption")]
    #[error(transparent)]
    OlmError(#[from] OlmError),

    /// An error occurred during a E2EE group operation.
    #[cfg(feature = "encryption")]
    #[error(transparent)]
    MegolmError(#[from] MegolmError),

    /// An error occurred during decryption.
    #[cfg(feature = "encryption")]
    #[error(transparent)]
    DecryptorError(#[from] DecryptorError),

    /// An error occurred in the state store.
    #[error(transparent)]
    StateStore(#[from] StoreError),

    /// An error encountered when trying to parse an identifier.
    #[error(transparent)]
    Identifier(#[from] IdentifierError),

    /// An error encountered when trying to parse a url.
    #[error(transparent)]
    Url(#[from] UrlParseError),

    /// An error while scanning a QR code.
    #[cfg(feature = "qrcode")]
    #[error(transparent)]
    QrCodeScanError(#[from] ScanError),
}

/// Error for the room key importing functionality.
#[cfg(feature = "encryption")]
#[derive(Error, Debug)]
// This is allowed because key importing isn't enabled under wasm.
#[allow(dead_code)]
pub enum RoomKeyImportError {
    /// An error de/serializing type for the `StateStore`
    #[error(transparent)]
    SerdeJson(#[from] JsonError),

    /// The cryptostore isn't yet open, logging in is required to open the
    /// cryptostore.
    #[error("The cryptostore hasn't been yet opened, can't import yet.")]
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

impl HttpError {
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
    pub fn uiaa_response(&self) -> Option<&UiaaInfo> {
        if let HttpError::UiaaError(FromHttpResponseError::Http(ServerError::Known(
            UiaaError::AuthResponse(i),
        ))) = self
        {
            Some(i)
        } else {
            None
        }
    }
}

impl Error {
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
    pub fn uiaa_response(&self) -> Option<&UiaaInfo> {
        if let Error::Http(HttpError::UiaaError(FromHttpResponseError::Http(ServerError::Known(
            UiaaError::AuthResponse(i),
        )))) = self
        {
            Some(i)
        } else {
            None
        }
    }
}

impl From<SdkBaseError> for Error {
    fn from(e: SdkBaseError) -> Self {
        match e {
            SdkBaseError::AuthenticationRequired => Self::AuthenticationRequired,
            SdkBaseError::StateStore(e) => Self::StateStore(e),
            SdkBaseError::SerdeJson(e) => Self::SerdeJson(e),
            SdkBaseError::IoError(e) => Self::Io(e),
            #[cfg(feature = "encryption")]
            SdkBaseError::CryptoStore(e) => Self::CryptoStoreError(e),
            #[cfg(feature = "encryption")]
            SdkBaseError::OlmError(e) => Self::OlmError(e),
            #[cfg(feature = "encryption")]
            SdkBaseError::MegolmError(e) => Self::MegolmError(e),
        }
    }
}

impl From<ReqwestError> for Error {
    fn from(e: ReqwestError) -> Self {
        Error::Http(HttpError::Reqwest(e))
    }
}
