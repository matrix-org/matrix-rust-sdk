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

use backoff::{future::retry, Error as RetryError, ExponentialBackoff};
use bytes::Bytes;
use bytesize::ByteSize;
use eyeball::SharedObservable;
use http::header::CONTENT_LENGTH;
use reqwest::Certificate;
use ruma::api::{error::FromHttpResponseError, IncomingResponse, OutgoingRequest};
use tracing::{debug, info, warn};

use super::{response_to_http_response, HttpClient, TransmissionProgress, DEFAULT_REQUEST_TIMEOUT};
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
        let backoff =
            ExponentialBackoff { max_elapsed_time: config.retry_timeout, ..Default::default() };
        let retry_count = AtomicU64::new(1);

        let send_request = || {
            let send_progress = send_progress.clone();
            async {
                debug!(num_attempt = retry_count.load(Ordering::SeqCst), "Sending request");

                let stop = if let Some(retry_limit) = config.retry_limit {
                    retry_count.fetch_add(1, Ordering::Relaxed) >= retry_limit
                } else {
                    false
                };

                // Turn errors into permanent errors when the retry limit is reached.
                let error_type = |err: HttpError| {
                    if stop {
                        RetryError::Permanent(err)
                    } else {
                        let has_retry_limit = config.retry_limit.is_some();
                        match err.retry_kind() {
                            RetryKind::Transient { retry_after } => {
                                RetryError::Transient { err, retry_after }
                            }
                            RetryKind::Permanent => RetryError::Permanent(err),
                            RetryKind::NetworkFailure => {
                                // If we ran into a network failure, only retry if there's some
                                // retry limit associated to this request's configuration;
                                // otherwise, we would end up running an infinite loop of network
                                // requests in offline mode.
                                if has_retry_limit {
                                    RetryError::Transient { err, retry_after: None }
                                } else {
                                    RetryError::Permanent(err)
                                }
                            }
                        }
                    }
                };

                let response = send_request(&self.inner, &request, config.timeout, send_progress)
                    .await
                    .map_err(error_type)?;

                let status_code = response.status();
                let response_size = ByteSize(response.body().len().try_into().unwrap_or(u64::MAX));
                tracing::Span::current()
                    .record("status", status_code.as_u16())
                    .record("response_size", response_size.to_string_as(true));

                // Record interesting headers. If you add more headers, ensure they're not
                // confidential.
                for (header_name, header_value) in response.headers() {
                    let header_name = header_name.as_str().to_lowercase();

                    // Header added in case of OIDC authentication failure, so we can correlate
                    // failures with a Sentry event emitted by the OIDC authentication server.
                    if header_name == "x-sentry-event-id" {
                        tracing::Span::current()
                            .record("sentry_event_id", header_value.to_str().unwrap_or("<???>"));
                    }
                }

                R::IncomingResponse::try_from_http_response(response)
                    .map_err(|e| error_type(HttpError::from(e)))
            }
        };

        retry::<_, HttpError, _, _, _>(backoff, send_request).await
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug)]
pub(crate) struct HttpSettings {
    pub(crate) disable_ssl_verification: bool,
    pub(crate) proxy: Option<String>,
    pub(crate) user_agent: Option<String>,
    pub(crate) timeout: Duration,
    pub(crate) additional_root_certificates: Vec<Certificate>,
    pub(crate) disable_built_in_root_certificates: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for HttpSettings {
    fn default() -> Self {
        Self {
            disable_ssl_verification: false,
            proxy: None,
            user_agent: None,
            timeout: DEFAULT_REQUEST_TIMEOUT,
            additional_root_certificates: Default::default(),
            disable_built_in_root_certificates: false,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl HttpSettings {
    /// Build a client with the specified configuration.
    pub(crate) fn make_client(&self) -> Result<reqwest::Client, HttpError> {
        let user_agent = self.user_agent.clone().unwrap_or_else(|| "matrix-rust-sdk".to_owned());
        let mut http_client =
            reqwest::Client::builder().user_agent(user_agent).timeout(self.timeout);

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
    timeout: Duration,
    send_progress: SharedObservable<TransmissionProgress>,
) -> Result<http::Response<Bytes>, HttpError> {
    use std::convert::Infallible;

    use futures_util::stream;

    let request = clone_request(request);
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

        *request.timeout_mut() = Some(timeout);
        request
    };

    let response = client.execute(request).await?;
    Ok(response_to_http_response(response).await?)
}

// Clones all request parts except the extensions which can't be cloned.
// See also https://github.com/hyperium/http/issues/395
fn clone_request(request: &http::Request<Bytes>) -> http::Request<Bytes> {
    let mut builder = http::Request::builder()
        .version(request.version())
        .method(request.method())
        .uri(request.uri());
    *builder.headers_mut().unwrap() = request.headers().clone();
    builder.body(request.body().clone()).unwrap()
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
