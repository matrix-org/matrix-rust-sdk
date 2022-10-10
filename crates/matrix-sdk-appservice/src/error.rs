// Copyright 2021 Famedly GmbH
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

use ruma::api::client::uiaa::UiaaInfo;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("missing access token")]
    MissingAccessToken,

    #[error("missing host on registration url")]
    MissingRegistrationHost,

    #[error("http request builder error")]
    UnknownHttpRequestBuilder,

    #[error("no port found")]
    MissingRegistrationPort,

    #[error("no client for localpart found")]
    NoClientForLocalpart,

    #[error("could not convert host:port to socket addr")]
    HostPortToSocketAddrs,

    #[error("uri has empty path")]
    UriEmptyPath,

    #[error("uri path is unknown")]
    UriPathUnknown,

    #[error("HTTP request parsing error: {0}")]
    FromHttpRequest(#[from] ruma::api::error::FromHttpRequestError),

    #[error("identifier failed to parse: {0}")]
    Identifier(#[from] ruma::IdParseError),

    #[error("HTTP error: {0}")]
    Http(#[from] http::Error),

    #[error("url parse error: {0}")]
    Url(#[from] url::ParseError),

    #[error("deserialization error: {0}")]
    Serde(#[from] serde::de::value::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("http uri invalid error: {0}")]
    InvalidUri(#[from] http::uri::InvalidUri),

    #[error(transparent)]
    Matrix(#[from] matrix_sdk::Error),

    #[error("regex error: {0}")]
    Regex(#[from] regex::Error),

    #[error("serde yaml error: {0}")]
    SerdeYaml(#[from] serde_yaml::Error),

    #[error("serde json error: {0}")]
    SerdeJson(#[from] serde_json::Error),

    #[error("utf8 error: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("hyper error: {0}")]
    Hyper(#[from] hyper::Error),
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
        match self {
            Error::Matrix(matrix) => matrix.uiaa_response(),
            _ => None,
        }
    }
}

impl From<matrix_sdk::HttpError> for Error {
    fn from(e: matrix_sdk::HttpError) -> Self {
        matrix_sdk::Error::from(e).into()
    }
}

impl From<matrix_sdk::StoreError> for Error {
    fn from(e: matrix_sdk::StoreError) -> Self {
        matrix_sdk::Error::from(e).into()
    }
}
