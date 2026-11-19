// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::sync::Arc;

use matrix_sdk::{HttpError, bytes::Bytes, reqwest::Certificate};
use matrix_sdk_base::http;
use ruma::api::error::ErrorBody;

use crate::client_builder::RequestConfig;

/// A standalone HTTP client, configured with the same [`HttpSettings`] and
/// [`RequestConfig`] as the ones used by a `Client`, including its retry
/// behaviour, certificates, user-agent, etc.
#[derive(uniffi::Object)]
struct HttpClient {
    inner: matrix_sdk::HttpClient,
}

#[matrix_sdk_ffi_macros::export]
impl HttpClient {
    #[uniffi::constructor]
    pub fn from_settings(
        http_settings: HttpSettings,
        request_config: RequestConfig,
    ) -> Result<Self, HttpClientError> {
        let inner =
            matrix_sdk::HttpClient::with_settings(http_settings.into(), request_config.into())
                .map_err(|error| HttpClientError::HttpClientCreation {
                    error: error.to_string(),
                })?;
        Ok(Self { inner })
    }

    /// Send a `GET` request to `url` and return the raw bytes of the
    /// response.
    ///
    ///  Note: When an HTTP error occurs, the error response can be found in the
    ///  `HttpClientError::GetUrl`'s `details` field.
    pub async fn get_url(&self, url: String) -> Result<Vec<u8>, HttpClientError> {
        let request = http::Request::get(&url)
            .body(Bytes::new())
            .map_err(|error| HttpClientError::FailedToFormatUrl { url, msg: error.to_string() })?;
        self.inner.send_raw(request, None).await.map_err(HttpClientError::from_get_url_error)
    }
}

#[derive(uniffi::Record, Default, Clone)]
pub struct HttpSettings {
    #[uniffi(default = false)]
    pub disable_ssl_verification: bool,
    #[uniffi(default = None)]
    pub proxy: Option<String>,
    #[uniffi(default = None)]
    pub user_agent: Option<String>,
    #[uniffi(default = None)]
    pub additional_root_certificates: Option<Vec<Arc<HttpCertificate>>>,
    #[uniffi(default = false)]
    pub disable_built_in_root_certificates: bool,
}

impl From<HttpSettings> for matrix_sdk::HttpSettings {
    fn from(value: HttpSettings) -> Self {
        let mut settings = Self::default();
        settings.disable_ssl_verification = value.disable_ssl_verification;
        settings.proxy = value.proxy;
        settings.user_agent = value.user_agent;
        settings.additional_root_certificates = value
            .additional_root_certificates
            .unwrap_or_default()
            .into_iter()
            .map(|cert| cert.inner.clone())
            .collect();
        settings.disable_built_in_root_certificates = value.disable_built_in_root_certificates;
        settings
    }
}

#[derive(uniffi::Object)]
pub struct HttpCertificate {
    inner: Certificate,
}

#[matrix_sdk_ffi_macros::export]
impl HttpCertificate {
    #[uniffi::constructor]
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, HttpClientError> {
        // We don't really know what type of certificate we may get
        // here, so let's try first one type, then the other.
        let inner = Certificate::from_der(&bytes).or_else(|der_error| {
            Certificate::from_pem(&bytes).map_err(|pem_error| {
                HttpClientError::FailedToFormatCertificate {
                    der_error: der_error.to_string(),
                    pem_error: pem_error.to_string(),
                }
            })
        })?;
        Ok(Self { inner })
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum HttpClientError {
    #[error("Failed to add a root certificate as DER ({der_error}) or PEM ({pem_error})")]
    FailedToFormatCertificate { der_error: String, pem_error: String },
    #[error("Failed to create HTTP client from settings: {error}")]
    HttpClientCreation { error: String },
    #[error("Failed to get URL content from url: {msg}")]
    GetUrl {
        msg: String,
        /// The HTTP status code of the response, if the server responded with
        /// an error. `None` if the request failed before getting a response.
        status_code: Option<u16>,
        /// The body of the response, if the server responded with an error.
        details: Option<String>,
    },
    #[error("Failed to parse {url} as a URL: {msg}")]
    FailedToFormatUrl { url: String, msg: String },
}

impl HttpClientError {
    fn from_get_url_error(error: HttpError) -> Self {
        let api_error = error.as_client_api_error();
        let status_code = api_error.map(|api_error| api_error.status_code.as_u16());
        let details = api_error.and_then(|api_error| match &api_error.body {
            ErrorBody::Standard(body) => serde_json::to_string(body).ok(),
            ErrorBody::Json(json) => Some(json.to_string()),
            ErrorBody::NotJson { bytes, .. } => Some(String::from_utf8_lossy(bytes).into_owned()),
        });

        Self::GetUrl { msg: error.to_string(), status_code, details }
    }
}
