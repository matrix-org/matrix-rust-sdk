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

use std::{
    any::type_name,
    fmt::Debug,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use bytesize::ByteSize;
use matrix_sdk_common::AsyncTraitDeps;
use ruma::{
    api::{
        error::{FromHttpResponseError, IntoHttpError},
        AuthScheme, IncomingResponse, MatrixVersion, OutgoingRequest, OutgoingRequestAppserviceExt,
        SendAccessToken,
    },
    UserId,
};
use tracing::{debug, field::debug, instrument, trace};

use crate::{config::RequestConfig, error::HttpError};

pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

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
    next_request_id: Arc<AtomicU64>,
}

impl HttpClient {
    pub(crate) fn new(inner: Arc<dyn HttpSend>, request_config: RequestConfig) -> Self {
        HttpClient { inner, request_config, next_request_id: AtomicU64::new(0).into() }
    }

    fn get_request_id(&self) -> String {
        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        format!("REQ-{request_id}")
    }

    fn serialize_request<R>(
        &self,
        request: R,
        config: RequestConfig,
        homeserver: String,
        access_token: Option<&str>,
        user_id: Option<&UserId>,
        server_versions: &[MatrixVersion],
    ) -> Result<http::Request<Bytes>, IntoHttpError>
    where
        R: OutgoingRequest + Debug,
    {
        trace!(request_type = type_name::<R>(), "Serializing request");

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

        Ok(request)
    }

    async fn send_request<R>(
        &self,
        request: http::Request<Bytes>,
        config: RequestConfig,
    ) -> Result<R::IncomingResponse, HttpError>
    where
        R: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<R::EndpointError>>,
    {
        // There's a bunch of state here, factor out a pinned inner future to
        // reduce this size of futures that await this function.
        #[cfg(not(target_arch = "wasm32"))]
        let (status_code, response_size, response) = Box::pin(async move {
            use backoff::{future::retry, Error as RetryError, ExponentialBackoff};
            use ruma::api::client::error::{
                ErrorBody as ClientApiErrorBody, ErrorKind as ClientApiErrorKind,
            };

            use crate::RumaApiError;

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
                        if let Some(api_error) = err.as_ruma_api_error() {
                            let status_code = match api_error {
                                RumaApiError::ClientApi(e) => match e.body {
                                    ClientApiErrorBody::Standard {
                                        kind: ClientApiErrorKind::LimitExceeded { retry_after_ms },
                                        ..
                                    } => {
                                        return RetryError::Transient {
                                            err,
                                            retry_after: retry_after_ms,
                                        };
                                    }
                                    _ => Some(e.status_code),
                                },
                                RumaApiError::Uiaa(_) => None,
                                RumaApiError::Other(e) => Some(e.status_code),
                            };

                            if let Some(status_code) = status_code {
                                if status_code.is_server_error() {
                                    return RetryError::Transient { err, retry_after: None };
                                }
                            }
                        }

                        RetryError::Permanent(err)
                    }
                };

                let response = self
                    .inner
                    .send_request(clone_request(&request), config.timeout)
                    .await
                    .map_err(error_type)?;

                let status_code = response.status();
                let response_size = response.body().len();

                let response = R::IncomingResponse::try_from_http_response(response)
                    .map_err(|e| error_type(HttpError::from(e)))?;

                Ok((status_code, response_size, response))
            };

            retry::<_, HttpError, _, _, _>(backoff, send_request).await
        })
        .await?;

        #[cfg(target_arch = "wasm32")]
        let (status_code, response_size, response) = {
            let response = self.inner.send_request(request, config.timeout).await?;
            let status_code = response.status();
            let response_size = response.body().len();

            (status_code, response_size, R::IncomingResponse::try_from_http_response(response)?)
        };

        let response_bytesize = ByteSize(response_size.try_into().unwrap_or(u64::MAX));
        tracing::Span::current()
            .record("status", status_code.as_u16())
            .record("response_size", response_bytesize.to_string_as(true));

        Ok(response)
    }

    #[instrument(
        skip(self, access_token, config, request, user_id),
        fields(
            config,
            path,
            user_id,
            request_size,
            request_body,
            request_id,
            status,
            response_size,
        )
    )]
    pub async fn send<R>(
        &self,
        request: R,
        config: Option<RequestConfig>,
        homeserver: String,
        access_token: Option<&str>,
        user_id: Option<&UserId>,
        server_versions: &[MatrixVersion],
    ) -> Result<R::IncomingResponse, HttpError>
    where
        R: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<R::EndpointError>>,
    {
        let config = match config {
            Some(config) => config,
            None => self.request_config,
        };

        // Keep some local variables in a separate scope so the compiler doesn't include
        // them in the future type. https://github.com/rust-lang/rust/issues/57478
        let request = {
            let request_id = self.get_request_id();
            let span = tracing::Span::current();

            // At this point in the code, the config isn't behind an Option anymore, that's
            // why we record it here, instead of in the #[instrument] macro.
            span.record("config", debug(config)).record("request_id", request_id);

            // The user ID is only used if we're an app-service. Only log the user_id if
            // it's `Some` and if assert_identity is set.
            if config.assert_identity {
                span.record("user_id", user_id.map(debug));
            }

            let auth_scheme = R::METADATA.authentication;
            if !matches!(auth_scheme, AuthScheme::AccessToken | AuthScheme::None) {
                return Err(HttpError::NotClientRequest);
            }

            let request = self.serialize_request(
                request,
                config,
                homeserver,
                access_token,
                user_id,
                server_versions,
            )?;

            let request_size = ByteSize(request.body().len().try_into().unwrap_or(u64::MAX));
            span.record("request_size", request_size.to_string_as(true));

            // Since sliding sync is experimental, and the proxy might not do what we expect
            // it to do given a specific request body, it's useful to log the
            // request body here. This doesn't contain any personal information.
            // TODO: Remove this once sliding sync isn't experimental anymore.
            #[cfg(feature = "experimental-sliding-sync")]
            if type_name::<R>() == "ruma_client_api::sync::sync_events::v4::Request" {
                span.record("request_body", debug(request.body()));
                span.record("path", request.uri().path_and_query().map(|p| p.as_str()));
            } else {
                span.record("path", request.uri().path());
            }

            #[cfg(not(feature = "experimental-sliding-sync"))]
            span.record("path", request.uri().path());

            request
        };

        debug!("Sending request");
        match self.send_request::<R>(request, config).await {
            Ok(response) => {
                debug!("Got response");
                Ok(response)
            }
            Err(e) => {
                debug!("Error while sending request: {e:?}");
                Err(e)
            }
        }
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
    mut response: reqwest::Response,
) -> Result<http::Response<Bytes>, reqwest::Error> {
    let status = response.status();

    let mut http_builder = http::Response::builder().status(status);
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
