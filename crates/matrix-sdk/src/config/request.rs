// Copyright 2021 The Matrix.org Foundation C.I.C.
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
    fmt::{self, Debug},
    num::NonZeroUsize,
    time::Duration,
};

use matrix_sdk_common::debug::DebugStructExt;
use ruma::api::MatrixVersion;

use crate::http_client::DEFAULT_REQUEST_TIMEOUT;

/// Configuration for requests the `Client` makes.
///
/// This sets how often and for how long a request should be repeated. As well
/// as how long a successful request is allowed to take.
///
/// By default requests are retried indefinitely and use no timeout.
///
/// # Examples
///
/// ```
/// use matrix_sdk::config::RequestConfig;
/// use std::time::Duration;
///
/// // This sets makes requests fail after a single send request and sets the timeout to 30s
/// let request_config = RequestConfig::new()
///     .disable_retry()
///     .timeout(Duration::from_secs(30));
/// ```
#[derive(Copy, Clone)]
pub struct RequestConfig {
    pub(crate) timeout: Duration,
    pub(crate) retry_limit: Option<u64>,
    pub(crate) retry_timeout: Option<Duration>,
    pub(crate) max_concurrent_requests: Option<NonZeroUsize>,
    pub(crate) force_auth: bool,
    pub(crate) force_matrix_version: Option<MatrixVersion>,
}

#[cfg(not(tarpaulin_include))]
impl Debug for RequestConfig {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            timeout,
            retry_limit,
            retry_timeout,
            force_auth,
            max_concurrent_requests,
            force_matrix_version,
        } = self;

        let mut res = fmt.debug_struct("RequestConfig");
        res.field("timeout", timeout)
            .maybe_field("retry_limit", retry_limit)
            .maybe_field("retry_timeout", retry_timeout)
            .maybe_field("max_concurrent_requests", max_concurrent_requests)
            .maybe_field("force_matrix_version", force_matrix_version);

        if *force_auth {
            res.field("force_auth", &true);
        }

        res.finish()
    }
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            timeout: DEFAULT_REQUEST_TIMEOUT,
            retry_limit: Default::default(),
            retry_timeout: Default::default(),
            max_concurrent_requests: Default::default(),
            force_auth: false,
            force_matrix_version: Default::default(),
        }
    }
}

impl RequestConfig {
    /// Create a new default `RequestConfig`.
    #[must_use]
    pub fn new() -> Self {
        Default::default()
    }

    /// Create a new `RequestConfig` with default values, except the retry limit
    /// which is set to 3.
    #[must_use]
    pub fn short_retry() -> Self {
        Self::default().retry_limit(3)
    }

    /// This is a convince method to disable the retries of a request. Setting
    /// the `retry_limit` to `0` has the same effect.
    #[must_use]
    pub fn disable_retry(mut self) -> Self {
        self.retry_limit = Some(0);
        self
    }

    /// The number of times a request should be retried. The default is no
    /// limit.
    #[must_use]
    pub fn retry_limit(mut self, retry_limit: u64) -> Self {
        self.retry_limit = Some(retry_limit);
        self
    }

    /// The total limit of request that are pending or run concurrently.
    /// Any additional request beyond that number will be waiting until another
    /// concurrent requests finished. Requests are queued fairly.
    #[must_use]
    pub fn max_concurrent_requests(mut self, limit: Option<NonZeroUsize>) -> Self {
        self.max_concurrent_requests = limit;
        self
    }

    /// Set the timeout duration for all HTTP requests.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set a timeout for how long a request should be retried. The default is
    /// no timeout, meaning requests are retried forever.
    #[must_use]
    pub fn retry_timeout(mut self, retry_timeout: Duration) -> Self {
        self.retry_timeout = Some(retry_timeout);
        self
    }

    /// Force sending authorization even if the endpoint does not require it.
    /// Default is only sending authorization if it is required.
    #[must_use]
    pub fn force_auth(mut self) -> Self {
        self.force_auth = true;
        self
    }

    /// Force the Matrix version used to select which version of the endpoint to
    /// use.
    ///
    /// Can be used to force the use of a stable endpoint when the versions
    /// advertised by the homeserver do not support it.
    #[must_use]
    pub(crate) fn force_matrix_version(mut self, version: MatrixVersion) -> Self {
        self.force_matrix_version = Some(version);
        self
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::RequestConfig;

    #[test]
    fn smoketest() {
        let cfg = RequestConfig::new()
            .force_auth()
            .retry_timeout(Duration::from_secs(32))
            .retry_limit(4)
            .timeout(Duration::from_secs(600));

        assert!(cfg.force_auth);
        assert_eq!(cfg.retry_limit, Some(4));
        assert_eq!(cfg.retry_timeout, Some(Duration::from_secs(32)));
        assert_eq!(cfg.timeout, Duration::from_secs(600));
    }

    #[test]
    fn testing_retry_settings() {
        let mut cfg = RequestConfig::new();
        assert_eq!(cfg.retry_limit, None);
        cfg = cfg.retry_limit(10);
        assert_eq!(cfg.retry_limit, Some(10));
        cfg = cfg.disable_retry();
        assert_eq!(cfg.retry_limit, Some(0));

        let cfg = RequestConfig::short_retry();
        assert_eq!(cfg.retry_limit, Some(3));
    }
}
