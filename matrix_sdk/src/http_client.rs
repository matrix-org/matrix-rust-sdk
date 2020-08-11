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

use std::{convert::TryFrom, sync::Arc};

use http::{HeaderValue, Method as HttpMethod, Response as HttpResponse};
use reqwest::{Client, Response};
use tracing::trace;
use url::Url;

use matrix_sdk_common::{locks::RwLock, FromHttpResponseError};

use crate::{ClientConfig, Endpoint, Error, HttpSend, Result, Session};

#[derive(Clone, Debug)]
pub(crate) struct HttpClient {
    pub(crate) inner: Arc<dyn HttpSend>,
    pub(crate) homeserver: Arc<Url>,
}

impl HttpClient {
    async fn send_request<Request: Endpoint>(
        &self,
        request: Request,
        session: Arc<RwLock<Option<Session>>>,
    ) -> Result<Response> {
        let mut request = {
            let read_guard;
            let access_token = if Request::METADATA.requires_authentication {
                read_guard = session.read().await;

                if let Some(session) = read_guard.as_ref() {
                    Some(session.access_token.as_str())
                } else {
                    return Err(Error::AuthenticationRequired);
                }
            } else {
                None
            };

            request.try_into_http_request(&self.homeserver.to_string(), access_token)?
        };

        if let HttpMethod::POST | HttpMethod::PUT | HttpMethod::DELETE = *request.method() {
            request.headers_mut().append(
                http::header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            );
        }

        self.inner.send_request(request).await
    }

    async fn response_to_http_response(
        &self,
        mut response: Response,
    ) -> Result<http::Response<Vec<u8>>> {
        let status = response.status();
        let mut http_builder = HttpResponse::builder().status(status);
        let headers = http_builder.headers_mut().unwrap();

        for (k, v) in response.headers_mut().drain() {
            if let Some(key) = k {
                headers.insert(key, v);
            }
        }
        let body = response.bytes().await?.as_ref().to_owned();
        Ok(http_builder.body(body).unwrap())
    }

    pub async fn send<Request>(
        &self,
        request: Request,
        session: Arc<RwLock<Option<Session>>>,
    ) -> Result<Request::IncomingResponse>
    where
        Request: Endpoint,
        Error: From<FromHttpResponseError<Request::ResponseError>>,
    {
        let response = self.send_request(request, session).await?;

        trace!("Got response: {:?}", response);

        let response = self.response_to_http_response(response).await?;

        Ok(Request::IncomingResponse::try_from(response)?)
    }
}

/// Default http client used if none is specified using `Client::with_client`.
#[derive(Clone, Debug)]
pub struct DefaultHttpClient {
    inner: Client,
}

impl DefaultHttpClient {
    /// Build a client with the specified configuration.
    pub fn with_config(config: &ClientConfig) -> Result<Self> {
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
        })
    }
}

#[async_trait::async_trait]
impl HttpSend for DefaultHttpClient {
    async fn send_request(&self, request: http::Request<Vec<u8>>) -> Result<Response> {
        Ok(self
            .inner
            .execute(reqwest::Request::try_from(request)?)
            .await?)
    }
}
