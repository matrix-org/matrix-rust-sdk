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

    #[error(transparent)]
    HttpRequest(#[from] ruma::api::error::FromHttpRequestError),

    #[error(transparent)]
    Identifier(#[from] ruma::identifiers::Error),

    #[error(transparent)]
    Http(#[from] http::Error),

    #[error(transparent)]
    Url(#[from] url::ParseError),

    #[error(transparent)]
    Serde(#[from] serde::de::value::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    InvalidUri(#[from] http::uri::InvalidUri),

    #[error(transparent)]
    Matrix(#[from] matrix_sdk::Error),

    #[error(transparent)]
    Regex(#[from] regex::Error),

    #[error(transparent)]
    SerdeYaml(#[from] serde_yaml::Error),

    #[cfg(feature = "actix")]
    #[error(transparent)]
    Actix(#[from] actix_web::Error),

    #[cfg(feature = "actix")]
    #[error(transparent)]
    ActixPayload(#[from] actix_web::error::PayloadError),
}

#[cfg(feature = "actix")]
impl actix_web::error::ResponseError for Error {}
