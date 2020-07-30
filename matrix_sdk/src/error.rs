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

use reqwest::Error as ReqwestError;
use serde_json::Error as JsonError;
use thiserror::Error;

use matrix_sdk_base::Error as MatrixError;

use crate::{
    api::{r0::uiaa::UiaaResponse as UiaaError, Error as RumaClientError},
    FromHttpResponseError as RumaResponseError, IntoHttpError as RumaIntoHttpError,
};

/// Result type of the rust-sdk.
pub type Result<T> = std::result::Result<T, Error>;

/// Internal representation of errors.
#[derive(Error, Debug)]
pub enum Error {
    /// Queried endpoint requires authentication but was called on an anonymous client.
    #[error("the queried endpoint requires authentication but was called before logging in")]
    AuthenticationRequired,

    /// An error at the HTTP layer.
    #[error(transparent)]
    Reqwest(#[from] ReqwestError),

    /// An error de/serializing type for the `StateStore`
    #[error(transparent)]
    SerdeJson(#[from] JsonError),

    /// An error converting between ruma_client_api types and Hyper types.
    #[error("can't parse the JSON response as a Matrix response")]
    RumaResponse(RumaResponseError<RumaClientError>),

    /// An error converting between ruma_client_api types and Hyper types.
    #[error("can't convert between ruma_client_api and hyper types.")]
    IntoHttp(RumaIntoHttpError),

    /// An error occurred in the Matrix client library.
    #[error(transparent)]
    MatrixError(#[from] MatrixError),

    /// An error occurred while authenticating.
    ///
    /// When registering or authenticating the Matrix server can send a `UiaaResponse`
    /// as the error type, this is a User-Interactive Authentication API response. This
    /// represents an error with information about how to authenticate the user.
    #[error("User-Interactive Authentication required.")]
    UiaaError(RumaResponseError<UiaaError>),
}

impl From<RumaResponseError<UiaaError>> for Error {
    fn from(error: RumaResponseError<UiaaError>) -> Self {
        Self::UiaaError(error)
    }
}

impl From<RumaResponseError<RumaClientError>> for Error {
    fn from(error: RumaResponseError<RumaClientError>) -> Self {
        Self::RumaResponse(error)
    }
}

impl From<RumaIntoHttpError> for Error {
    fn from(error: RumaIntoHttpError) -> Self {
        Self::IntoHttp(error)
    }
}
