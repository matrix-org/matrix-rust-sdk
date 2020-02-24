// Copyright 2020 Damir Jelić
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

use std::error::Error as StdError;
use std::fmt::{Display, Formatter, Result as FmtResult};

use reqwest::Error as ReqwestError;
use ruma_api::error::FromHttpResponseError as RumaResponseError;
use ruma_api::error::IntoHttpError as RumaIntoHttpError;
use url::ParseError;

/// An error that can occur during client operations.
#[derive(Debug)]
pub struct Error(pub(crate) InnerError);

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        let message = match self.0 {
            InnerError::AuthenticationRequired => "The queried endpoint requires authentication but was called with an anonymous client.",
            InnerError::Reqwest(_) => "An HTTP error occurred.",
            InnerError::Uri(_) => "Provided string could not be converted into a URI.",
            InnerError::RumaResponseError(_) => "An error occurred converting between ruma_client_api and hyper types.",
            InnerError::IntoHttpError(_) => "An error occurred converting between ruma_client_api and hyper types.",
        };

        write!(f, "{}", message)
    }
}

impl StdError for Error {}

/// Internal representation of errors.
#[derive(Debug)]
pub(crate) enum InnerError {
    /// Queried endpoint requires authentication but was called on an anonymous client.
    AuthenticationRequired,
    /// An error at the HTTP layer.
    Reqwest(ReqwestError),
    /// An error when parsing a string as a URI.
    Uri(ParseError),
    /// An error converting between ruma_client_api types and Hyper types.
    RumaResponseError(RumaResponseError),
    /// An error converting between ruma_client_api types and Hyper types.
    IntoHttpError(RumaIntoHttpError),
}

impl From<ParseError> for Error {
    fn from(error: ParseError) -> Self {
        Self(InnerError::Uri(error))
    }
}

impl From<RumaResponseError> for Error {
    fn from(error: RumaResponseError) -> Self {
        Self(InnerError::RumaResponseError(error))
    }
}

impl From<RumaIntoHttpError> for Error {
    fn from(error: RumaIntoHttpError) -> Self {
        Self(InnerError::IntoHttpError(error))
    }
}

impl From<ReqwestError> for Error {
    fn from(error: ReqwestError) -> Self {
        Self(InnerError::Reqwest(error))
    }
}
