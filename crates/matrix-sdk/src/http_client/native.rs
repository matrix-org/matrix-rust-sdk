// Copyright 2023 The Matrix.org Foundation C.I.C.
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
    fmt::Debug,
    mem,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use backon::{ExponentialBuilder, Retryable};
use bytes::Bytes;
use bytesize::ByteSize;
use eyeball::SharedObservable;
use http::header::CONTENT_LENGTH;
use reqwest::{Certificate, tls};
use ruma::api::{IncomingResponse, OutgoingRequest, error::FromHttpResponseError};
use tracing::{debug, info, warn};

use super::{DEFAULT_REQUEST_TIMEOUT, HttpClient, TransmissionProgress, response_to_http_response};
use crate::{
    config::RequestConfig,
    error::{HttpError, RetryKind},
};

impl HttpClient {
    pub(super) async fn send_request<R>(
        &self,
        request: http::Request<Bytes>,
        config: RequestConfig,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<R::IncomingResponse, HttpError>
    where
        R: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<R::EndpointError>>,
    {
        // These values were picked because we used to use the `backoff` crate, those
        // were defined here: https://docs.rs/backoff/0.4.0/backoff/default/index.html
        let backoff = ExponentialBuilder::new()
            .with_min_delay(Duration::from_millis(500))
            .with_max_delay(Duration::from_secs(60))
            .with_total_delay(Some(Duration::from_secs(15 * 60)))
            .without_max_times();

        // Let's now apply any override the user or the SDK might have set.
        let backoff = if let Some(max_delay) = config.max_retry_time {
            backoff.with_max_delay(max_delay)
        } else {
            backoff
        };

        let backoff = if let Some(max_times) = config.retry_limit {
            // Backon behaves a bit differently to our own handcrafted max retry logic.
            // We were counting from one while `backon` counts from zero.
            backoff.with_max_times(max_times.saturating_sub(1))
        } else {
            backoff
        };

        let retry_count = AtomicU64::new(1);

        let send_request = || {
            let send_progress = send_progress.clone();

            async {
                let num_attempt = retry_count.fetch_add(1, Ordering::SeqCst);
                debug!(num_attempt, "Sending request");
                let before = ruma::time::Instant::now();

                let response =
                    send_request(&self.inner, &request, config.timeout, send_progress).await?;

                let request_duration = ruma::time::Instant::now().saturating_duration_since(before);

                let status_code = response.status();
                let response_size = ByteSize(response.body().len().try_into().unwrap_or(u64::MAX));
                tracing::Span::current()
                    .record("status", status_code.as_u16())
                    .record("response_size", response_size.display().si_short().to_string())
                    .record("request_duration", tracing::field::debug(request_duration));

                // Record interesting headers. If you add more headers, ensure they're not
                // confidential.
                for (header_name, header_value) in response.headers() {
                    let header_name = header_name.as_str().to_lowercase();

                    // Header added in case of OAuth 2.0 authentication failure, so we can correlate
                    // failures with a Sentry event emitted by the OAuth 2.0 authentication server.
                    if header_name == "x-sentry-event-id" {
                        tracing::Span::current()
                            .record("sentry_event_id", header_value.to_str().unwrap_or("<???>"));
                    }
                }

                R::IncomingResponse::try_from_http_response(response).map_err(HttpError::from)
            }
        };

        let has_retry_limit = config.retry_limit.is_some();

        send_request
            .retry(backoff)
            .adjust(|err, backon_suggested_timeout| {
                match err.retry_kind() {
                    RetryKind::Transient { retry_after } => {
                        // This bit is somewhat tricky but it's necessary so we respect the
                        // `max_times` limit from `backon`.
                        //
                        // The exponential backoff in `backon` is implemented as an iterator that
                        // returns `None` when we hit the `max_times` limit; if it returned `None`,
                        // that means we ran out of attempts. So it's necessary to only override
                        // the `backon_suggested_timeout` if it's `Some`.
                        if backon_suggested_timeout.is_some() {
                            retry_after.or(backon_suggested_timeout)
                        } else {
                            None
                        }
                    }
                    RetryKind::Permanent => None,
                    RetryKind::NetworkFailure => {
                        // If we ran into a network failure, only retry if there's some retry limit
                        // associated to this request's configuration; otherwise, we would end up
                        // running an infinite loop of network requests in offline mode.
                        if has_retry_limit { backon_suggested_timeout } else { None }
                    }
                }
            })
            .await
    }
}

#[cfg(not(target_family = "wasm"))]
#[derive(Clone, Debug)]
pub(crate) struct HttpSettings {
    pub(crate) disable_ssl_verification: bool,
    pub(crate) proxy: Option<String>,
    pub(crate) user_agent: Option<String>,
    pub(crate) timeout: Option<Duration>,
    pub(crate) read_timeout: Option<Duration>,
    pub(crate) additional_root_certificates: Vec<Certificate>,
    pub(crate) disable_built_in_root_certificates: bool,
}

#[cfg(not(target_family = "wasm"))]
impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            disable_ssl_verification: false,
            proxy: None,
            user_agent: None,
            timeout: Some(DEFAULT_REQUEST_TIMEOUT),
            read_timeout: None,
            additional_root_certificates: Default::default(),
            disable_built_in_root_certificates: false,
        }
    }
}

#[cfg(not(target_family = "wasm"))]
impl HttpSettings {
    /// Build a client with the specified configuration.
    pub(crate) fn make_client(&self) -> Result<reqwest::Client, HttpError> {
        let user_agent = self.user_agent.clone().unwrap_or_else(|| "matrix-rust-sdk".to_owned());
        let mut http_client = reqwest::Client::builder()
            .user_agent(user_agent)
            // As recommended by BCP 195.
            // See: https://datatracker.ietf.org/doc/bcp195/
            .min_tls_version(tls::Version::TLS_1_2);

        if let Some(timeout) = self.timeout {
            http_client = http_client.timeout(timeout);
        }

        if let Some(read_timeout) = self.read_timeout {
            http_client = http_client.read_timeout(read_timeout);
        }

        if self.disable_ssl_verification {
            warn!("SSL verification disabled in the HTTP client!");
            http_client = http_client.danger_accept_invalid_certs(true)
        }

        if !self.additional_root_certificates.is_empty() {
            info!(
                "Adding {} additional root certificates to the HTTP client",
                self.additional_root_certificates.len()
            );

            for cert in &self.additional_root_certificates {
                http_client = http_client.add_root_certificate(cert.clone());
            }
        }

        if self.disable_built_in_root_certificates {
            info!("Built-in root certificates disabled in the HTTP client.");
            http_client = http_client.tls_built_in_root_certs(false);
        }

        if let Some(p) = &self.proxy {
            info!(proxy_url = p, "Setting the proxy for the HTTP client");
            http_client = http_client.proxy(reqwest::Proxy::all(p.as_str())?);
        }

        Ok(http_client.build()?)
    }
}

pub(super) async fn send_request(
    client: &reqwest::Client,
    request: &http::Request<Bytes>,
    timeout: Option<Duration>,
    send_progress: SharedObservable<TransmissionProgress>,
) -> Result<http::Response<Bytes>, HttpError> {
    use std::convert::Infallible;

    use futures_util::stream;

    let request = request.clone();
    let request = {
        let mut request = if send_progress.subscriber_count() != 0 {
            let content_length = request.body().len();
            send_progress.update(|p| p.total += content_length);

            // Make sure any concurrent futures in the same task get a chance
            // to also add to the progress total before the first chunks are
            // pulled out of the body stream.
            tokio::task::yield_now().await;

            let mut req = reqwest::Request::try_from(request.map(|body| {
                let chunks = stream::iter(BytesChunks::new(body, 8192).map(
                    move |chunk| -> Result<_, Infallible> {
                        send_progress.update(|p| p.current += chunk.len());
                        Ok(chunk)
                    },
                ));
                reqwest::Body::wrap_stream(chunks)
            }))?;

            // When streaming the request, reqwest / hyper doesn't know how
            // large the body is, so it doesn't set the content-length header
            // (required by some servers). Set it manually.
            req.headers_mut().insert(CONTENT_LENGTH, content_length.into());

            req
        } else {
            reqwest::Request::try_from(request)?
        };

        *request.timeout_mut() = timeout;
        request
    };

    let response = client.execute(request).await?;
    Ok(response_to_http_response(response).await?)
}

struct BytesChunks {
    bytes: Bytes,
    size: usize,
}

impl BytesChunks {
    fn new(bytes: Bytes, size: usize) -> Self {
        assert_ne!(size, 0);
        Self { bytes, size }
    }
}

impl Iterator for BytesChunks {
    type Item = Bytes;

    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.is_empty() {
            None
        } else if self.bytes.len() < self.size {
            Some(mem::take(&mut self.bytes))
        } else {
            Some(self.bytes.split_to(self.size))
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::BytesChunks;

    #[test]
    fn test_bytes_chunks() {
        let bytes = Bytes::new();
        assert!(BytesChunks::new(bytes, 1).collect::<Vec<_>>().is_empty());

        let bytes = Bytes::from_iter([1, 2]);
        assert_eq!(BytesChunks::new(bytes, 2).collect::<Vec<_>>(), [Bytes::from_iter([1, 2])]);

        let bytes = Bytes::from_iter([1, 2]);
        assert_eq!(BytesChunks::new(bytes, 3).collect::<Vec<_>>(), [Bytes::from_iter([1, 2])]);

        let bytes = Bytes::from_iter([1, 2, 3]);
        assert_eq!(
            BytesChunks::new(bytes, 1).collect::<Vec<_>>(),
            [Bytes::from_iter([1]), Bytes::from_iter([2]), Bytes::from_iter([3])]
        );

        let bytes = Bytes::from_iter([1, 2, 3]);
        assert_eq!(
            BytesChunks::new(bytes, 2).collect::<Vec<_>>(),
            [Bytes::from_iter([1, 2]), Bytes::from_iter([3])]
        );

        let bytes = Bytes::from_iter([1, 2, 3, 4]);
        assert_eq!(
            BytesChunks::new(bytes, 2).collect::<Vec<_>>(),
            [Bytes::from_iter([1, 2]), Bytes::from_iter([3, 4])]
        );
    }
}
