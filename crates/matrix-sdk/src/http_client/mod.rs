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
    borrow::Cow,
    fmt::Debug,
    num::NonZeroUsize,
    sync::{
        Arc, RwLock as StdRwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use bytesize::ByteSize;
use eyeball::SharedObservable;
use http::Method;
use matrix_sdk_base::SendOutsideWasm;
use ruma::api::{
    OutgoingRequest, OutgoingRequestExt, SupportedVersions,
    auth_scheme::{self, AuthScheme, SendAccessToken},
    error::{FromHttpResponseError, IntoHttpError},
    path_builder,
};
use tokio::sync::{Semaphore, SemaphorePermit, watch};
use tracing::{Instrument, debug, error, field::debug, trace, warn};

use crate::{HttpResult, config::RequestConfig, error::HttpError};

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod wasm;

#[cfg(not(target_family = "wasm"))]
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

/// Cumulative process-lifetime traffic counters, for launch/bandwidth
/// instrumentation. Counts HTTP body bytes per attempt (retries count each
/// time, as they cost bandwidth each time); headers are not included.
#[derive(Debug, Default)]
pub struct TrafficCounters {
    pub(crate) uploaded_bytes: AtomicU64,
    pub(crate) downloaded_bytes: AtomicU64,
    pub(crate) request_count: AtomicU64,
}

/// A point-in-time snapshot of [`TrafficCounters`].
#[derive(Clone, Copy, Debug)]
pub struct TrafficStats {
    /// Total HTTP request body bytes sent since the client was built.
    pub uploaded_bytes: u64,
    /// Total HTTP response body bytes received since the client was built.
    pub downloaded_bytes: u64,
    /// Number of HTTP request attempts (retries counted individually).
    pub request_count: u64,
}

impl TrafficCounters {
    pub(crate) fn snapshot(&self) -> TrafficStats {
        TrafficStats {
            uploaded_bytes: self.uploaded_bytes.load(Ordering::Relaxed),
            downloaded_bytes: self.downloaded_bytes.load(Ordering::Relaxed),
            request_count: self.request_count.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct HttpClient {
    /// The underlying `reqwest` client. Swapped for a fresh one (new
    /// connection pool) on [`HttpClient::handle_network_change`], so always go
    /// through [`HttpClient::reqwest`] rather than caching a clone.
    inner: Arc<StdRwLock<reqwest::Client>>,
    /// The settings the client was built from, if any, used to rebuild it on a
    /// network change. `None` when the caller supplied its own `reqwest`
    /// client.
    #[cfg(not(target_family = "wasm"))]
    settings: Option<HttpSettings>,
    /// Bumped on every network change; in-flight requests race against it and
    /// re-send themselves on the fresh connection pool.
    network_change: watch::Sender<u64>,
    pub(crate) request_config: RequestConfig,
    pub(crate) traffic: Arc<TrafficCounters>,
    concurrent_request_semaphore: MaybeSemaphore,
    next_request_id: Arc<AtomicU64>,
}

impl HttpClient {
    pub(crate) fn new(inner: reqwest::Client, request_config: RequestConfig) -> Self {
        HttpClient {
            inner: Arc::new(StdRwLock::new(inner)),
            #[cfg(not(target_family = "wasm"))]
            settings: None,
            network_change: watch::Sender::new(0),
            request_config,
            traffic: Arc::new(TrafficCounters::default()),
            concurrent_request_semaphore: MaybeSemaphore::new(
                request_config.max_concurrent_requests,
            ),
            next_request_id: AtomicU64::new(0).into(),
        }
    }

    /// Remember the settings the `reqwest` client was built from, so it can be
    /// rebuilt on a network change.
    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn with_settings(mut self, settings: Option<HttpSettings>) -> Self {
        self.settings = settings;
        self
    }

    /// The current underlying `reqwest` client.
    pub(crate) fn reqwest(&self) -> reqwest::Client {
        self.inner.read().unwrap().clone()
    }

    /// The OS reported that the network path changed (e.g. Wi-Fi to cellular).
    ///
    /// Connections bound to the old interface may be black-holed rather than
    /// closed, so nothing would notice until the request timeout fires. Drop
    /// the connection pool by rebuilding the `reqwest` client (when we know how
    /// it was built), then wake every in-flight request so it re-sends itself
    /// immediately on a fresh connection, without consuming a retry attempt.
    pub(crate) fn handle_network_change(&self) {
        #[cfg(not(target_family = "wasm"))]
        if let Some(settings) = &self.settings {
            match settings.make_client() {
                Ok(client) => *self.inner.write().unwrap() = client,
                Err(err) => {
                    warn!("Failed to rebuild the HTTP client after a network change: {err}")
                }
            }
        }

        self.network_change.send_modify(|generation| *generation += 1);
        debug!("Network change: in-flight requests will be re-sent");
    }

    /// Subscribe to network-change notifications, for racing an in-flight
    /// request against them.
    pub(super) fn subscribe_network_change(&self) -> watch::Receiver<u64> {
        self.network_change.subscribe()
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
        path_builder_input: <R::PathBuilder as path_builder::PathBuilder>::Input<'_>,
    ) -> Result<http::Request<Bytes>, IntoHttpError>
    where
        R: OutgoingRequest + Debug,
        R::Authentication: SupportedAuthScheme,
    {
        trace!(request_type = type_name::<R>(), "Serializing request");

        let send_access_token = match access_token {
            Some(access_token) => match (config.force_auth, config.skip_auth) {
                (true, true) | (true, false) => SendAccessToken::Always(access_token),
                (false, true) => SendAccessToken::None,
                (false, false) => SendAccessToken::IfRequired(access_token),
            },
            None => SendAccessToken::None,
        };
        let authentication_input = R::Authentication::authentication_input(send_access_token);

        let request = request
            .try_into_http_request::<BytesMut>(
                &homeserver,
                authentication_input,
                path_builder_input,
            )?
            .map(|body| body.freeze());

        Ok(request)
    }

    pub fn send<R>(
        &self,
        request: R,
        config: Option<RequestConfig>,
        homeserver: String,
        access_token: Option<&str>,
        path_builder_input: <R::PathBuilder as path_builder::PathBuilder>::Input<'_>,
        send_progress: SharedObservable<TransmissionProgress>,
        recv_progress: SharedObservable<TransmissionProgress>,
    ) -> impl Future<Output = Result<R::IncomingResponse, HttpError>>
    where
        R: OutgoingRequest + Debug,
        R::Authentication: SupportedAuthScheme,
        HttpError: From<FromHttpResponseError<R::EndpointError>>,
    {
        // some functions split out so they only get compiled once,
        // not monomorphized per request type
        fn make_span(client: &HttpClient, config: &RequestConfig) -> tracing::Span {
            tracing::info_span!(
                "send",
                uri = tracing::field::Empty,
                ?config,
                method = tracing::field::Empty,
                request_id = client.get_request_id(),
                request_size = tracing::field::Empty,
                request_duration = tracing::field::Empty,
                status = tracing::field::Empty,
                response_size = tracing::field::Empty,
                sentry_event_id = tracing::field::Empty
            )
        }
        fn record_request_uri_and_size(request: &http::Request<Bytes>) {
            let method = request.method();

            let mut uri_parts = request.uri().clone().into_parts();

            // Erase the query parameters for the sake of secrecy (in case a token is
            // present).
            if let Some(path_and_query) = &mut uri_parts.path_and_query {
                *path_and_query =
                    path_and_query.path().try_into().expect("path is valid PathAndQuery");
            }

            let uri = http::Uri::from_parts(uri_parts).expect("created from valid URI");

            let span = tracing::Span::current();
            span.record("method", debug(method)).record("uri", uri.to_string());

            // POST, PUT, PATCH are the only methods that are reasonably used
            // in conjunction with request bodies
            if [Method::POST, Method::PUT, Method::PATCH].contains(method) {
                let request_size = request.body().len().try_into().unwrap_or(u64::MAX);
                span.record(
                    "request_size",
                    ByteSize(request_size).display().si_short().to_string(),
                );
            }
        }
        // these macros expand to a lot of code, also want to skip monomorphization
        // for them even though they might look super simple
        fn log_got_response() {
            debug!("Got response");
        }
        fn log_error(e: &HttpError) {
            error!("Error while sending request: {e:?}");
        }

        let config = match config {
            Some(config) => config,
            None => self.request_config,
        };

        async move {
            let request = self
                .serialize_request(request, config, homeserver, access_token, path_builder_input)
                .map_err(HttpError::IntoHttp)?;
            record_request_uri_and_size(&request);

            // will be automatically dropped at the end of this function
            let _handle = self.concurrent_request_semaphore.acquire().await;

            // There's a bunch of state in send_request, factor out a pinned inner
            // future to reduce the size of futures that await this function.
            match Box::pin(self.send_request::<R>(request, config, send_progress, recv_progress))
                .await
            {
                Ok(response) => {
                    log_got_response();
                    Ok(response)
                }
                Err(e) => {
                    log_error(&e);
                    Err(e)
                }
            }
        }
        .instrument(make_span(self, &config))
    }
}

/// Progress of sending or receiving a payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransmissionProgress {
    /// How many bytes were already transferred.
    pub current: usize,
    /// How many bytes there are in total.
    pub total: usize,
}

/// Reads the response body. On native, when `recv_progress` has subscribers,
/// the body is streamed chunk by chunk so they see the download progress (the
/// total is the `Content-Length` when the server sends one).
async fn response_to_http_response(
    mut response: reqwest::Response,
    recv_progress: SharedObservable<TransmissionProgress>,
) -> Result<http::Response<Bytes>, reqwest::Error> {
    let status = response.status();

    let mut http_builder = http::Response::builder().status(status);
    let headers = http_builder.headers_mut().expect("Can't get the response builder headers");

    for (k, v) in response.headers_mut().drain() {
        if let Some(key) = k {
            headers.insert(key, v);
        }
    }

    #[cfg(not(target_family = "wasm"))]
    let body = if recv_progress.subscriber_count() != 0 {
        if let Some(length) = response.content_length() {
            recv_progress.update(|p| p.total += length.try_into().unwrap_or(usize::MAX));
        }
        let mut body = BytesMut::new();
        while let Some(chunk) = response.chunk().await? {
            recv_progress.update(|p| p.current += chunk.len());
            body.extend_from_slice(&chunk);
        }
        body.freeze()
    } else {
        response.bytes().await?
    };
    #[cfg(target_family = "wasm")]
    let body = {
        let _ = recv_progress;
        response.bytes().await?
    };

    Ok(http_builder.body(body).expect("Can't construct a response using the given body"))
}

/// Marker trait to identify the authentication schemes that the
/// [`Client`](crate::Client) supports.
///
/// This trait can also be implemented for custom [`AuthScheme`]s if necessary.
pub trait SupportedAuthScheme: AuthScheme {
    /// Get the [`AuthScheme::Input`] from the access token.
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_>;
}

impl SupportedAuthScheme for auth_scheme::NoAccessToken {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::AccessToken {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::AccessTokenOptional {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::AppserviceToken {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::AppserviceTokenOptional {
    fn authentication_input(access_token: SendAccessToken<'_>) -> Self::Input<'_> {
        access_token
    }
}

impl SupportedAuthScheme for auth_scheme::NoAuthentication {
    fn authentication_input(_access_token: SendAccessToken<'_>) -> Self::Input<'_> {}
}

/// Marker trait to identify the path builders that the
/// [`Client`](crate::Client) supports.
///
/// This trait can also be implemented for custom
/// [`PathBuilder`](path_builder::PathBuilder)s if necessary.
pub trait SupportedPathBuilder: path_builder::PathBuilder {
    /// Get the [`PathBuilder::Input`](path_builder::PathBuilder::Input) from
    /// the [`Client`](crate::Client).
    fn get_path_builder_input(
        client: &crate::Client,
        skip_auth: bool,
    ) -> impl Future<Output = HttpResult<Self::Input<'static>>> + SendOutsideWasm;
}

impl SupportedPathBuilder for path_builder::VersionHistory {
    async fn get_path_builder_input(
        client: &crate::Client,
        skip_auth: bool,
    ) -> HttpResult<Cow<'static, SupportedVersions>> {
        // We always enable "failsafe" mode for the GET /versions requests in this
        // function. It disables trying to refresh the access token for those requests,
        // to avoid possible deadlocks.

        if !client.auth_ctx().has_valid_access_token() {
            // Try to get the value in the cache.
            if let Ok(Some(versions)) = client.supported_versions_cached().await {
                return Ok(Cow::Owned(versions));
            }

            // The request will skip auth so we might not get all the supported features, so
            // just fetch the supported versions and don't cache them.
            let response = client.fetch_server_versions_inner(true, None).await?;

            Ok(Cow::Owned(response.as_supported_versions()))
        } else if skip_auth {
            let cached_versions = client.supported_versions_cached().await;

            let versions = if let Ok(Some(versions)) = cached_versions {
                versions
            } else {
                // If we're skipping auth we might not get all the supported features, so just
                // fetch the versions and don't cache them.
                let request_config = RequestConfig::default().retry_limit(5).skip_auth();
                let response =
                    client.fetch_server_versions_inner(true, Some(request_config)).await?;

                response.as_supported_versions()
            };

            Ok(Cow::Owned(versions))
        } else {
            client.supported_versions_inner(true).await.map(Cow::Owned)
        }
    }
}

impl SupportedPathBuilder for path_builder::SinglePath {
    async fn get_path_builder_input(_client: &crate::Client, _skip_auth: bool) -> HttpResult<()> {
        Ok(())
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::{
        num::NonZeroUsize,
        sync::{
            Arc,
            atomic::{AtomicU8, Ordering},
        },
        time::{Duration, Instant},
    };

    use matrix_sdk_common::executor::spawn;
    use matrix_sdk_test::{async_test, test_json};
    use wiremock::{
        Mock, Request, ResponseTemplate,
        matchers::{method, path},
    };

    use crate::{
        http_client::RequestConfig,
        test_utils::{set_client_session, test_client_builder_with_server},
    };

    #[async_test]
    async fn test_ensure_concurrent_request_limit_is_observed() {
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

        let bg_task = spawn(async move {
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
    async fn test_ensure_no_max_concurrent_request_does_not_limit() {
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

        let bg_task = spawn(async move {
            futures_util::future::join_all((0..254).map(|_| client.whoami())).await
        });

        // give it some time to issue the requests
        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(counter.load(Ordering::SeqCst), 254, "Not all requests passed through");
        bg_task.abort();
    }

    #[async_test]
    async fn test_network_change_resends_in_flight_request() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder.build().await.unwrap();

        set_client_session(&client).await;

        let counter = Arc::new(AtomicU8::new(0));
        let inner_counter = counter.clone();

        // The first attempt is black-holed (it would only answer long after the
        // request timeout)...
        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(move |_req: &Request| {
                inner_counter.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_delay(Duration::from_secs(60))
            })
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // ...while the re-send after the network change is answered at once.
        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "user_id": "@joe:example.org" })),
            )
            .mount(&server)
            .await;

        let bg_client = client.clone();
        let bg_task = spawn(async move { bg_client.whoami().await });

        // Let the first attempt get in flight, then flip the network.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1, "The first attempt should be in flight");

        client.notify_network_change();

        let response = tokio::time::timeout(Duration::from_secs(5), bg_task)
            .await
            .expect("the in-flight request must be re-sent right away, not wait for its timeout")
            .unwrap()
            .unwrap();
        assert_eq!(response.user_id, "@joe:example.org");
    }

    #[async_test]
    async fn test_network_change_resends_tracked_transfer_only_once_stalled() {
        let (client_builder, server) = test_client_builder_with_server().await;
        let client = client_builder.build().await.unwrap();

        set_client_session(&client).await;

        let counter = Arc::new(AtomicU8::new(0));
        let inner_counter = counter.clone();

        // The first attempt is black-holed...
        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(move |_req: &Request| {
                inner_counter.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_delay(Duration::from_secs(60))
            })
            .up_to_n_times(1)
            .mount(&server)
            .await;

        // ...while the re-send is answered at once.
        Mock::given(method("GET"))
            .and(path("_matrix/client/r0/account/whoami"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "user_id": "@joe:example.org" })),
            )
            .mount(&server)
            .await;

        // A request whose transfer is being watched (like an upload with a
        // progress bar) gets the stall grace before it's given up on.
        let progress = eyeball::SharedObservable::new(super::TransmissionProgress::default());
        let _watcher = progress.subscribe();
        let bg_client = client.clone();
        let bg_task = spawn(async move {
            bg_client
                .send(ruma::api::client::account::whoami::v3::Request::new())
                .with_send_progress_observable(progress)
                .await
        });

        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(counter.load(Ordering::SeqCst), 1, "The first attempt should be in flight");

        let flipped_at = Instant::now();
        client.notify_network_change();

        let response = tokio::time::timeout(Duration::from_secs(5), bg_task)
            .await
            .expect(
                "the stalled transfer must be re-sent after the grace, not wait for its timeout",
            )
            .unwrap()
            .unwrap();
        assert_eq!(response.user_id, "@joe:example.org");
        assert!(
            flipped_at.elapsed() >= super::native::NETWORK_CHANGE_STALL_GRACE,
            "a watched transfer must be given the grace period before being re-sent"
        );
    }
}
