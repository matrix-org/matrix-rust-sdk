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

use std::{convert::TryFrom, fmt::Debug, sync::Arc};

use bytes::{Bytes, BytesMut};
use http::Response as HttpResponse;
use matrix_sdk_common::{async_trait, locks::RwLock, AsyncTraitDeps};
use reqwest::{Client, Response};
use ruma::api::{
    client::media::create_content, error::FromHttpResponseError, AuthScheme, IncomingResponse,
    MatrixVersion, OutgoingRequest, OutgoingRequestAppserviceExt, SendAccessToken,
};
use tracing::trace;
use url::Url;

use crate::{
    config::{ClientConfig, RequestConfig},
    error::HttpError,
    Session,
};

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
    /// * `request_config` - The config used for this request.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::convert::TryFrom;
    /// use matrix_sdk::{HttpSend, async_trait, HttpError, config::RequestConfig, bytes::Bytes};
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
    ///         config: RequestConfig,
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
        config: RequestConfig,
    ) -> Result<http::Response<Bytes>, HttpError>;
}

#[derive(Clone, Debug)]
pub(crate) struct HttpClient {
    pub(crate) inner: Arc<dyn HttpSend>,
    pub(crate) homeserver: Arc<RwLock<Url>>,
    pub(crate) session: Arc<RwLock<Option<Session>>>,
    pub(crate) request_config: RequestConfig,
}

impl HttpClient {
    pub(crate) fn new(
        inner: Arc<dyn HttpSend>,
        homeserver: Arc<RwLock<Url>>,
        session: Arc<RwLock<Option<Session>>>,
        request_config: RequestConfig,
    ) -> Self {
        HttpClient { inner, homeserver, session, request_config }
    }

    async fn send_request<Request: OutgoingRequest>(
        &self,
        request: Request,
        session: Arc<RwLock<Option<Session>>>,
        config: Option<RequestConfig>,
    ) -> Result<http::Response<Bytes>, HttpError> {
        let config = match config {
            Some(config) => config,
            None => self.request_config,
        };

        let auth_scheme = Request::METADATA.authentication;
        if !matches!(auth_scheme, AuthScheme::AccessToken | AuthScheme::None) {
            return Err(HttpError::NotClientRequest);
        }

        let access_token;

        let request = if !self.request_config.assert_identity {
            let send_access_token = if auth_scheme == AuthScheme::None && !config.force_auth {
                // Small optimization: Don't take the session lock if we know the auth token
                // isn't going to be used anyways.
                SendAccessToken::None
            } else {
                match session.read().await.as_ref() {
                    Some(session) => {
                        access_token = session.access_token.clone();
                        if config.force_auth {
                            SendAccessToken::Always(&access_token)
                        } else {
                            SendAccessToken::IfRequired(&access_token)
                        }
                    }
                    None => SendAccessToken::None,
                }
            };

            request.try_into_http_request::<BytesMut>(
                &self.homeserver.read().await.to_string(),
                send_access_token,
                // FIXME: Use versions reported by server
                &[MatrixVersion::V1_0],
            )?
        } else {
            let (send_access_token, user_id) = {
                let session = session.read().await;
                let session = session.as_ref().ok_or(HttpError::UserIdRequired)?;

                access_token = session.access_token.clone();
                (SendAccessToken::Always(&access_token), session.user_id.clone())
            };

            request.try_into_http_request_with_user_id::<BytesMut>(
                &self.homeserver.read().await.to_string(),
                send_access_token,
                &user_id,
                // FIXME: Use versions reported by server
                &[MatrixVersion::V1_0],
            )?
        };

        let request = request.map(|body| body.freeze());
        self.inner.send_request(request, config).await
    }

    pub async fn upload(
        &self,
        request: create_content::v3::Request<'_>,
        config: Option<RequestConfig>,
    ) -> Result<create_content::v3::Response, HttpError> {
        let response = self.send_request(request, self.session.clone(), config).await?;
        Ok(create_content::v3::Response::try_from_http_response(response)?)
    }

    pub async fn send<Request>(
        &self,
        request: Request,
        config: Option<RequestConfig>,
    ) -> Result<Request::IncomingResponse, HttpError>
    where
        Request: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<Request::EndpointError>>,
    {
        let response = self.send_request(request, self.session.clone(), config).await?;

        trace!("Got response: {:?}", response);

        let response = Request::IncomingResponse::try_from_http_response(response)?;

        Ok(response)
    }
}

/// Build a client with the specified configuration.
pub(crate) fn client_with_config(config: &ClientConfig) -> Result<Client, HttpError> {
    let http_client = reqwest::Client::builder();

    #[cfg(not(target_arch = "wasm32"))]
    let http_client = {
        let http_client = if config.disable_ssl_verification {
            http_client.danger_accept_invalid_certs(true)
        } else {
            http_client
        };

        let http_client = match &config.proxy {
            Some(p) => http_client.proxy(p.clone()),
            None => http_client,
        };

        let user_agent = config.user_agent.clone().unwrap_or_else(|| "matrix-rust-sdk".to_owned());

        http_client.user_agent(user_agent).timeout(config.request_config.timeout)
    };

    #[cfg(target_arch = "wasm32")]
    #[allow(unused)]
    let _ = config;

    Ok(http_client.build()?)
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

#[cfg(any(target_arch = "wasm32"))]
async fn send_request(
    client: &Client,
    request: http::Request<Bytes>,
    _: RequestConfig,
) -> Result<http::Response<Bytes>, HttpError> {
    let request = reqwest::Request::try_from(request)?;
    let response = client.execute(request).await?;

    Ok(response_to_http_response(response).await?)
}

#[cfg(all(not(target_arch = "wasm32")))]
async fn send_request(
    client: &Client,
    request: http::Request<Bytes>,
    config: RequestConfig,
) -> Result<http::Response<Bytes>, HttpError> {
    use std::sync::atomic::{AtomicU64, Ordering};

    use backoff::{future::retry, Error as RetryError, ExponentialBackoff};
    use http::StatusCode;

    let mut backoff = ExponentialBackoff::default();
    let mut request = reqwest::Request::try_from(request)?;
    let retry_limit = config.retry_limit;
    let retry_count = AtomicU64::new(1);

    *request.timeout_mut() = Some(config.timeout);

    backoff.max_elapsed_time = config.retry_timeout;

    let request = &request;
    let retry_count = &retry_count;

    let request = || async move {
        let stop = if let Some(retry_limit) = retry_limit {
            retry_count.fetch_add(1, Ordering::Relaxed) >= retry_limit
        } else {
            false
        };

        // Turn errors into permanent errors when the retry limit is reached
        let error_type = if stop {
            RetryError::Permanent
        } else {
            |err| RetryError::Transient { err, retry_after: None }
        };

        let request = request.try_clone().ok_or(HttpError::UnableToCloneRequest)?;

        let response =
            client.execute(request).await.map_err(|e| error_type(HttpError::Reqwest(e)))?;

        let status_code = response.status();
        // TODO TOO_MANY_REQUESTS will have a retry timeout which we should
        // use.
        if !stop
            && (status_code.is_server_error() || response.status() == StatusCode::TOO_MANY_REQUESTS)
        {
            return Err(error_type(HttpError::Server(status_code)));
        }

        let response = response_to_http_response(response)
            .await
            .map_err(|e| RetryError::Permanent(HttpError::Reqwest(e)))?;

        Ok(response)
    };

    let response = retry(backoff, request).await?;

    Ok(response)
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl HttpSend for Client {
    async fn send_request(
        &self,
        request: http::Request<Bytes>,
        config: RequestConfig,
    ) -> Result<http::Response<Bytes>, HttpError> {
        send_request(self, request, config).await
    }
}
