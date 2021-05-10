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

use http::StatusCode;
use matrix_sdk_base::{Error as MatrixError, StoreError};
use matrix_sdk_common::{
    api::{
        r0::uiaa::{UiaaInfo, UiaaResponse as UiaaError},
        Error as RumaClientError,
    },
    identifiers::Error as IdentifierError,
    FromHttpResponseError, IntoHttpError, ServerError,
};
use reqwest::Error as ReqwestError;
use serde_json::Error as JsonError;
use std::io::Error as IoError;
use thiserror::Error;

#[cfg(feature = "encryption")]
use matrix_sdk_base::crypto::store::CryptoStoreError;

/// Result type of the rust-sdk.
pub type Result<T> = std::result::Result<T, Error>;

/// An HTTP error, representing either a connection error or an error while
/// converting the raw HTTP response into a Matrix response.
#[derive(Error, Debug)]
pub enum HttpError {
    /// An error at the HTTP layer.
    #[error(transparent)]
    Reqwest(#[from] ReqwestError),

    /// Queried endpoint requires authentication but was called on an anonymous client.
    #[error("the queried endpoint requires authentication but was called before logging in")]
    AuthenticationRequired,

    /// Client tried to force authentication but did not provide an access token.
    #[error("tried to force authentication but no access token was provided")]
    ForcedAuthenticationWithoutAccessToken,

    /// Queried endpoint is not meant for clients.
    #[error("the queried endpoint is not meant for clients")]
    NotClientRequest,

    /// An error converting between ruma_client_api types and Hyper types.
    #[error(transparent)]
    FromHttpResponse(#[from] FromHttpResponseError<RumaClientError>),

    /// An error converting between ruma_client_api types and Hyper types.
    #[error(transparent)]
    IntoHttp(#[from] IntoHttpError),

    /// An error occurred while authenticating.
    ///
    /// When registering or authenticating the Matrix server can send a `UiaaResponse`
    /// as the error type, this is a User-Interactive Authentication API response. This
    /// represents an error with information about how to authenticate the user.
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
    #[cfg(feature = "appservice")]
    UserIdRequired,
}

/// Internal representation of errors.
#[derive(Error, Debug)]
pub enum Error {
    /// Error doing an HTTP request.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// Queried endpoint requires authentication but was called on an anonymous client.
    #[error("the queried endpoint requires authentication but was called before logging in")]
    AuthenticationRequired,

    /// An error de/serializing type for the `StateStore`
    #[error(transparent)]
    SerdeJson(#[from] JsonError),

    /// An IO error happened.
    #[error(transparent)]
    Io(#[from] IoError),

    /// An error occurred in the Matrix client library.
    #[error(transparent)]
    MatrixError(#[from] MatrixError),

    /// An error occurred in the crypto store.
    #[cfg(feature = "encryption")]
    #[error(transparent)]
    CryptoStoreError(#[from] CryptoStoreError),

    /// An error occured in the state store.
    #[error(transparent)]
    StateStore(#[from] StoreError),

    /// An error encountered when trying to parse an identifier.
    #[error(transparent)]
    Identifier(#[from] IdentifierError),
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

impl From<ReqwestError> for Error {
    fn from(e: ReqwestError) -> Self {
        Error::Http(HttpError::Reqwest(e))
    }
}
