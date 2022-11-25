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

use std::{any::type_name, fmt::Debug, sync::Arc, time::Duration};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use http::Response as HttpResponse;
use matrix_sdk_common::AsyncTraitDeps;
use reqwest::Response;
use ruma::{
    api::{
        error::FromHttpResponseError, AuthScheme, IncomingResponse, MatrixVersion, OutgoingRequest,
        OutgoingRequestAppserviceExt, SendAccessToken,
    },
    UserId,
};
use tracing::trace;

use crate::{config::RequestConfig, error::HttpError};

pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Abstraction around the http layer. The allows implementors to use different
/// http libraries.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
pub trait HttpSend: AsyncTraitDeps {
    /// The method abstracting sending request types and receiving response
    /// types.
    ///
    /// This is called by the client every time it wants to send anything to a
    /// homeserver.
    ///
    /// # Arguments
    ///
    /// * `request` - The http request that has been converted from a ruma
    ///   `Request`.
    ///
    /// * `timeout` - A timeout for the full request > response cycle.
    /// # Examples
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use matrix_sdk::{async_trait, bytes::Bytes, HttpError, HttpSend};
    ///
    /// #[derive(Debug)]
    /// struct Client(reqwest::Client);
    ///
    /// impl Client {
    ///     async fn response_to_http_response(
    ///         &self,
    ///         mut response: reqwest::Response,
    ///     ) -> Result<http::Response<Bytes>, HttpError> {
    ///         // Convert the reqwest response to a http one.
    ///         todo!()
    ///     }
    /// }
    ///
    /// #[async_trait]
    /// impl HttpSend for Client {
    ///     async fn send_request(
    ///         &self,
    ///         request: http::Request<Bytes>,
    ///         timeout: Duration,
    ///     ) -> Result<http::Response<Bytes>, HttpError> {
    ///         Ok(self
    ///             .response_to_http_response(
    ///                 self.0
    ///                     .execute(reqwest::Request::try_from(request)?)
    ///                     .await?,
    ///             )
    ///             .await?)
    ///     }
    /// }
    /// ```
    async fn send_request(
        &self,
        request: http::Request<Bytes>,
        timeout: Duration,
    ) -> Result<http::Response<Bytes>, HttpError>;
}

#[derive(Debug)]
pub(crate) struct HttpClient {
    pub(crate) inner: Arc<dyn HttpSend>,
    pub(crate) request_config: RequestConfig,
}

impl HttpClient {
    pub(crate) fn new(inner: Arc<dyn HttpSend>, request_config: RequestConfig) -> Self {
        HttpClient { inner, request_config }
    }

    #[tracing::instrument(
        skip(self, request, access_token),
        fields(request_type = type_name::<Request>()),
    )]
    pub async fn send<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
        homeserver: String,
        access_token: Option<&str>,
        user_id: Option<&UserId>,
        server_versions: &[MatrixVersion],
    ) -> Result<Request::IncomingResponse, HttpError>
    where
        Request: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        let config = match config {
            Some(config) => config,
            None => self.request_config,
        };

        let auth_scheme = Request::METADATA.authentication;
        if !matches!(auth_scheme, AuthScheme::AccessToken | AuthScheme::None) {
            return Err(HttpError::NotClientRequest);
        }

        trace!("Serializing request");
        // We can't assert the identity without a user_id.
        let request = if let Some((access_token, user_id)) =
            access_token.filter(|_| config.assert_identity).zip(user_id)
        {
            request.try_into_http_request_with_user_id::<BytesMut>(
                &homeserver,
                SendAccessToken::Always(access_token),
                user_id,
                server_versions,
            )?
        } else {
            let send_access_token = match access_token {
                Some(access_token) => {
                    if config.force_auth {
                        SendAccessToken::Always(access_token)
                    } else {
                        SendAccessToken::IfRequired(access_token)
                    }
                }
                None => SendAccessToken::None,
            };

            request.try_into_http_request::<BytesMut>(
                &homeserver,
                send_access_token,
                server_versions,
            )?
        };

        let request = request.map(|body| body.freeze());

        trace!("Sending request");

        #[cfg(not(target_arch = "wasm32"))]
        let response = {
            use std::sync::atomic::{AtomicU64, Ordering};

            use backoff::{future::retry, Error as RetryError, ExponentialBackoff};
            use ruma::api::client::error::ErrorKind as ClientApiErrorKind;

            let backoff =
                ExponentialBackoff { max_elapsed_time: config.retry_timeout, ..Default::default() };
            let retry_count = AtomicU64::new(1);

            let send_request = || async {
                let stop = if let Some(retry_limit) = config.retry_limit {
                    retry_count.fetch_add(1, Ordering::Relaxed) >= retry_limit
                } else {
                    false
                };

                // Turn errors into permanent errors when the retry limit is reached
                let error_type = if stop {
                    RetryError::Permanent
                } else {
                    |err: HttpError| {
                        let retry_after = err.client_api_error_kind().and_then(|kind| match kind {
                            ClientApiErrorKind::LimitExceeded { retry_after_ms } => *retry_after_ms,
                            _ => None,
                        });
                        RetryError::Transient { err, retry_after }
                    }
                };

                let raw_response = self
                    .inner
                    .send_request(clone_request(&request), config.timeout)
                    .await
                    .map_err(error_type)?;

                trace!("Got response: {raw_response:?}");

                let response = Request::IncomingResponse::try_from_http_response(raw_response)
                    .map_err(|e| error_type(HttpError::from(e)))?;

                Ok(response)
            };

            retry::<_, HttpError, _, _, _>(backoff, send_request).await?
        };

        #[cfg(target_arch = "wasm32")]
        let response = {
            let raw_response = self.inner.send_request(request, config.timeout).await?;
            trace!("Got response: {raw_response:?}");

            Request::IncomingResponse::try_from_http_response(raw_response)?
        };

        Ok(response)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpSettings {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) disable_ssl_verification: bool,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) proxy: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) user_agent: Option<String>,
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) timeout: Duration,
}

#[allow(clippy::derivable_impls)]
impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            disable_ssl_verification: false,
            #[cfg(not(target_arch = "wasm32"))]
            proxy: None,
            #[cfg(not(target_arch = "wasm32"))]
            user_agent: None,
            #[cfg(not(target_arch = "wasm32"))]
            timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }
}

impl HttpSettings {
    /// Build a client with the specified configuration.
    pub(crate) fn make_client(&self) -> Result<reqwest::Client, HttpError> {
        #[allow(unused_mut)]
        let mut http_client = reqwest::Client::builder();

        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.disable_ssl_verification {
                http_client = http_client.danger_accept_invalid_certs(true)
            }

            if let Some(p) = &self.proxy {
                http_client = http_client.proxy(reqwest::Proxy::all(p.as_str())?);
            }

            let user_agent =
                self.user_agent.clone().unwrap_or_else(|| "matrix-rust-sdk".to_owned());

            http_client = http_client.user_agent(user_agent).timeout(self.timeout);
        };

        Ok(http_client.build()?)
    }
}

// Clones all request parts except the extensions which can't be cloned.
// See also https://github.com/hyperium/http/issues/395
#[cfg(not(target_arch = "wasm32"))]
fn clone_request(request: &http::Request<Bytes>) -> http::Request<Bytes> {
    let mut builder = http::Request::builder()
        .version(request.version())
        .method(request.method())
        .uri(request.uri());
    *builder.headers_mut().unwrap() = request.headers().clone();
    builder.body(request.body().clone()).unwrap()
}

async fn response_to_http_response(
    mut response: Response,
) -> Result<http::Response<Bytes>, reqwest::Error> {
    let status = response.status();

    let mut http_builder = HttpResponse::builder().status(status);
    let headers = http_builder.headers_mut().expect("Can't get the response builder headers");

    for (k, v) in response.headers_mut().drain() {
        if let Some(key) = k {
            headers.insert(key, v);
        }
    }

    let body = response.bytes().await?;

    Ok(http_builder.body(body).expect("Can't construct a response using the given body"))
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl HttpSend for reqwest::Client {
    async fn send_request(
        &self,
        request: http::Request<Bytes>,
        _timeout: Duration,
    ) -> Result<http::Response<Bytes>, HttpError> {
        #[allow(unused_mut)]
        let mut request = reqwest::Request::try_from(request)?;

        // reqwest's timeout functionality is not available on WASM
        #[cfg(not(target_arch = "wasm32"))]
        {
            *request.timeout_mut() = Some(_timeout);
        }

        let response = self.execute(request).await?;

        Ok(response_to_http_response(response).await?)
    }
}
