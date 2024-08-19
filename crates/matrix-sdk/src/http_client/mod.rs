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
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use bytesize::ByteSize;
use eyeball::SharedObservable;
use http::Method;
use ruma::api::{
    error::{FromHttpResponseError, IntoHttpError},
    AuthScheme, MatrixVersion, OutgoingRequest, SendAccessToken,
};
use tokio::sync::{Semaphore, SemaphorePermit};
use tracing::{debug, field::debug, instrument, trace};

use crate::{config::RequestConfig, error::HttpError};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::HttpSettings;

pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug)]
struct MaybeSemaphore(Arc<Option<Semaphore>>);

#[allow(dead_code)] // false-positive lint: we never use it but only hold it for the drop
struct MaybeSemaphorePermit<'a>(Option<SemaphorePermit<'a>>);

impl MaybeSemaphore {
    fn new(max: Option<NonZeroUsize>) -> Self {
        let inner = max.map(|i| Semaphore::new(i.into()));
        MaybeSemaphore(Arc::new(inner))
    }

    async fn acquire(&self) -> MaybeSemaphorePermit<'_> {
        match self.0.as_ref() {
            Some(inner) => {
                // This can only ever error if the semaphore was closed,
                // which we never do, so we can safely ignore any error case
                MaybeSemaphorePermit(inner.acquire().await.ok())
            }
            None => MaybeSemaphorePermit(None),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpClient {
    pub(crate) inner: reqwest::Client,
    pub(crate) request_config: RequestConfig,
    concurrent_request_semaphore: MaybeSemaphore,
    next_request_id: Arc<AtomicU64>,
}

impl HttpClient {
    pub(crate) fn new(inner: reqwest::Client, request_config: RequestConfig) -> Self {
        HttpClient {
            inner,
            request_config,
            concurrent_request_semaphore: MaybeSemaphore::new(
                request_config.max_concurrent_requests,
            ),
            next_request_id: AtomicU64::new(0).into(),
        }
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
        server_versions: &[MatrixVersion],
    ) -> Result<http::Request<Bytes>, IntoHttpError>
    where
        R: OutgoingRequest + Debug,
    {
        trace!(request_type = type_name::<R>(), "Serializing request");

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

        let request = request
            .try_into_http_request::<BytesMut>(&homeserver, send_access_token, server_versions)?
            .map(|body| body.freeze());

        Ok(request)
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(
        skip(self, request, config, homeserver, access_token, send_progress),
        fields(
            config,
            uri,
            method,
            request_size,
            request_body,
            request_id,
            status,
            response_size,
            sentry_event_id,
        )
    )]
    pub async fn send<R>(
        &self,
        request: R,
        config: Option<RequestConfig>,
        homeserver: String,
        access_token: Option<&str>,
        server_versions: &[MatrixVersion],
        send_progress: SharedObservable<TransmissionProgress>,
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

            let auth_scheme = R::METADATA.authentication;
            match auth_scheme {
                AuthScheme::AccessToken
                | AuthScheme::AccessTokenOptional
                | AuthScheme::AppserviceToken
                | AuthScheme::None => {}
                AuthScheme::ServerSignatures => {
                    return Err(HttpError::NotClientRequest);
                }
            }

            let request = self
                .serialize_request(request, config, homeserver, access_token, server_versions)
                .map_err(HttpError::IntoHttp)?;

            let method = request.method();

            let mut uri_parts = request.uri().clone().into_parts();
            if let Some(path_and_query) = &mut uri_parts.path_and_query {
                *path_and_query =
                    path_and_query.path().try_into().expect("path is valid PathAndQuery");
            }
            let uri = http::Uri::from_parts(uri_parts).expect("created from valid URI");

            span.record("method", debug(method)).record("uri", uri.to_string());

            // POST, PUT, PATCH are the only methods that are reasonably used
            // in conjunction with request bodies
            if [Method::POST, Method::PUT, Method::PATCH].contains(method) {
                let request_size = request.body().len().try_into().unwrap_or(u64::MAX);
                span.record("request_size", ByteSize(request_size).to_string_as(true));
            }

            // Since sliding sync is experimental, and the proxy might not do what we expect
            // it to do given a specific request body, it's useful to log the
            // request body here. This doesn't contain any personal information.
            // TODO: Remove this once sliding sync isn't experimental anymore.
            #[cfg(feature = "experimental-sliding-sync")]
            if type_name::<R>() == "ruma_client_api::sync::sync_events::v4::Request" {
                span.record("request_body", debug(request.body()));
            }

            request
        };

        // will be automatically dropped at the end of this function
        let _handle = self.concurrent_request_semaphore.acquire().await;

        // There's a bunch of state in send_request, factor out a pinned inner
        // future to reduce this size of futures that await this function.
        match Box::pin(self.send_request::<R>(request, config, send_progress)).await {
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

/// Progress of sending or receiving a payload.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransmissionProgress {
    /// How many bytes were already transferred.
    pub current: usize,
    /// How many bytes there are in total.
    pub total: usize,
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

#[cfg(feature = "experimental-oidc")]
impl tower::Service<http::Request<Bytes>> for HttpClient {
    type Response = http::Response<Bytes>;
    type Error = tower::BoxError;
    type Future = futures_core::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<Bytes>) -> Self::Future {
        let inner = self.inner.clone();

        let fut = async move {
            native::send_request(&inner, &req, DEFAULT_REQUEST_TIMEOUT, Default::default())
                .await
                .map_err(Into::into)
        };
        Box::pin(fut)
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::{
        num::NonZeroUsize,
        sync::{
            atomic::{AtomicU8, Ordering},
            Arc,
        },
        time::Duration,
    };

    use matrix_sdk_test::{async_test, test_json};
    use wiremock::{
        matchers::{method, path},
        Mock, Request, ResponseTemplate,
    };

    use crate::{
        http_client::RequestConfig,
        test_utils::{set_client_session, test_client_builder_with_server},
    };

    #[async_test]
    async fn ensure_concurrent_request_limit_is_observed() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder
            .request_config(RequestConfig::default().max_concurrent_requests(NonZeroUsize::new(5)))
            .build()
            .await
            .unwrap();

        set_client_session(&client).await;

        let counter = Arc::new(AtomicU8::new(0));
        let inner_counter = counter.clone();

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(move |_req: &Request| {
                inner_counter.fetch_add(1, Ordering::SeqCst);
                // we stall the requests
                ResponseTemplate::new(200).set_delay(Duration::from_secs(60))
            })
            .mount(&server)
            .await;

        let bg_task = tokio::spawn(async move {
            futures_util::future::join_all((0..10).map(|_| client.whoami())).await
        });

        // give it some time to issue the requests
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            5,
            "More requests passed than the limit we configured"
        );
        bg_task.abort();
    }

    #[async_test]
    async fn ensure_no_max_concurrent_request_does_not_limit() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder
            .request_config(RequestConfig::default().max_concurrent_requests(None))
            .build()
            .await
            .unwrap();

        set_client_session(&client).await;

        let counter = Arc::new(AtomicU8::new(0));
        let inner_counter = counter.clone();

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(move |_req: &Request| {
                inner_counter.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_delay(Duration::from_secs(60))
            })
            .mount(&server)
            .await;

        let bg_task = tokio::spawn(async move {
            futures_util::future::join_all((0..254).map(|_| client.whoami())).await
        });

        // give it some time to issue the requests
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 254, "Not all requests passed through");
        bg_task.abort();
    }
}
