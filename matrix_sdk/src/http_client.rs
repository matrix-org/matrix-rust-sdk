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

use std::fmt::{self, Debug, Formatter};
use std::sync::Arc;

use http::Method as HttpMethod;
use reqwest::{header::HeaderValue, header::AUTHORIZATION, Client, Response};
use url::Url;

use matrix_sdk_common::locks::RwLock;

use matrix_sdk_base::Session;
use matrix_sdk_common_macros::async_trait;

use crate::{ClientConfig, Error, Result};

/// Abstraction around the http layer. The allows implementors to use different
/// http libraries.
#[async_trait]
pub trait HttpClient: Sync + Send {
    /// The method abstracting sending request types and receiving response types.
    ///
    /// This is called by the client every time it wants to send anything to a homeserver.
    async fn send_request(
        &self,
        requires_auth: bool,
        method: HttpMethod,
        homeserver: &Url,
        session: &Arc<RwLock<Option<Session>>>,
        request: http::Request<Vec<u8>>,
    ) -> Result<Response>;
}

impl Debug for dyn HttpClient {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "<dyn HttpClient>")
    }
}
/// Default http client used if none is specified using `Client::with_client`.
#[derive(Clone, Debug)]
pub struct DefaultHttpClient {
    inner: Client,
    homeserver: Arc<Url>,
}

impl DefaultHttpClient {
    /// Returns a `DefaultHttpClient` built with the default config.
    pub fn new(homeserver: Arc<Url>) -> Self {
        Self::with_config(&ClientConfig::default(), homeserver).unwrap()
    }

    /// Build a client with the specified configuration.
    pub fn with_config(config: &ClientConfig, homeserver: Arc<Url>) -> Result<Self> {
        let http_client = reqwest::Client::builder();

        #[cfg(not(target_arch = "wasm32"))]
        let http_client = {
            let http_client = match config.timeout {
                Some(x) => http_client.timeout(x),
                None => http_client,
            };

            let http_client = if config.disable_ssl_verification {
                http_client.danger_accept_invalid_certs(true)
            } else {
                http_client
            };

            let http_client = match &config.proxy {
                Some(p) => http_client.proxy(p.clone()),
                None => http_client,
            };

            let mut headers = reqwest::header::HeaderMap::new();

            let user_agent = match &config.user_agent {
                Some(a) => a.clone(),
                None => {
                    HeaderValue::from_str(&format!("matrix-rust-sdk {}", crate::VERSION)).unwrap()
                }
            };

            headers.insert(reqwest::header::USER_AGENT, user_agent);

            http_client.default_headers(headers)
        };

        Ok(Self {
            inner: http_client.build()?,
            homeserver,
        })
    }
}

#[async_trait]
impl HttpClient for DefaultHttpClient {
    async fn send_request(
        &self,
        requires_auth: bool,
        method: HttpMethod,
        homeserver: &Url,
        session: &Arc<RwLock<Option<Session>>>,
        request: http::Request<Vec<u8>>,
    ) -> Result<Response> {
        let url = request.uri();
        let path_and_query = url.path_and_query().unwrap();
        let mut url = homeserver.clone();

        url.set_path(path_and_query.path());
        url.set_query(path_and_query.query());

        let request_builder = match method {
            HttpMethod::GET => self.inner.get(url),
            HttpMethod::POST => {
                let body = request.body().clone();
                self.inner
                    .post(url)
                    .body(body)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
            }
            HttpMethod::PUT => {
                let body = request.body().clone();
                self.inner
                    .put(url)
                    .body(body)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
            }
            HttpMethod::DELETE => {
                let body = request.body().clone();
                self.inner
                    .delete(url)
                    .body(body)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
            }
            method => panic!("Unsupported method {}", method),
        };

        let request_builder = if requires_auth {
            if let Some(session) = session.read().await.as_ref() {
                let header_value = format!("Bearer {}", &session.access_token);
                request_builder.header(AUTHORIZATION, header_value)
            } else {
                return Err(Error::AuthenticationRequired);
            }
        } else {
            request_builder
        };

        Ok(request_builder.send().await?)
    }
}
