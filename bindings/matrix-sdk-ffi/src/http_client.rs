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

use std::{sync::Arc, time::Duration};

use matrix_sdk::reqwest::{self, Certificate};

/// A standalone HTTP client, configured with the same [`HttpSettings`] as the
/// one used by a `Client`.
#[derive(uniffi::Object)]
struct HttpClient {
    inner: reqwest::Client,
}

#[matrix_sdk_ffi_macros::export]
impl HttpClient {
    #[uniffi::constructor]
    pub fn from_settings(http_settings: HttpSettings) -> Result<Self, HttpClientError> {
        let inner = matrix_sdk::HttpSettings::from(http_settings)
            .make_client()
            .map_err(|error| HttpClientError::HttpClientCreation { error: error.to_string() })?;
        Ok(Self { inner })
    }

    pub async fn get_url(&self, url: String) -> Result<Vec<u8>, HttpClientError> {
        let response = self.inner.get(url).send().await?;
        if response.status().is_success() {
            Ok(response.bytes().await?.into())
        } else {
            Err(HttpClientError::GetUrl {
                msg: response.status().to_string(),
                details: response.text().await.ok(),
            })
        }
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
    pub timeout: Option<Duration>,
    #[uniffi(default = None)]
    pub read_timeout: Option<Duration>,
    #[uniffi(default = None)]
    pub additional_root_certificates: Option<Vec<Arc<HttpCertificate>>>,
    #[uniffi(default = false)]
    pub disable_built_in_root_certificates: bool,
}

impl From<HttpSettings> for matrix_sdk::HttpSettings {
    fn from(value: HttpSettings) -> Self {
        Self {
            disable_ssl_verification: value.disable_ssl_verification,
            proxy: value.proxy,
            user_agent: value.user_agent,
            timeout: value.timeout,
            read_timeout: value.read_timeout,
            additional_root_certificates: value
                .additional_root_certificates
                .unwrap_or_default()
                .into_iter()
                .map(|cert| cert.inner.clone())
                .collect(),
            disable_built_in_root_certificates: value.disable_built_in_root_certificates,
        }
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
    GetUrl { msg: String, details: Option<String> },
}

impl From<reqwest::Error> for HttpClientError {
    fn from(err: reqwest::Error) -> Self {
        Self::GetUrl { msg: err.to_string(), details: None }
    }
}
