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
    pub(crate) timeout: Option<Duration>,
    pub(crate) read_timeout: Option<Duration>,
    pub(crate) retry_limit: Option<usize>,
    pub(crate) max_retry_time: Option<Duration>,
    pub(crate) max_concurrent_requests: Option<NonZeroUsize>,
    pub(crate) force_auth: bool,
    pub(crate) skip_auth: bool,
}

#[cfg(not(tarpaulin_include))]
impl Debug for RequestConfig {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            timeout,
            read_timeout,
            retry_limit,
            max_retry_time: retry_timeout,
            force_auth,
            max_concurrent_requests,
            skip_auth: skip_optional_auth,
        } = self;

        let mut res = fmt.debug_struct("RequestConfig");
        res.field("timeout", timeout)
            .maybe_field("read_timeout", read_timeout)
            .maybe_field("retry_limit", retry_limit)
            .maybe_field("max_retry_time", retry_timeout)
            .maybe_field("max_concurrent_requests", max_concurrent_requests);

        if *force_auth {
            res.field("force_auth", &true);
        }

        if *skip_optional_auth {
            res.field("skip_optional_auth", &true);
        }

        res.finish()
    }
}

impl Default for RequestConfig {
    fn default() -> Self {
        Self {
            timeout: Some(DEFAULT_REQUEST_TIMEOUT),
            read_timeout: None,
            retry_limit: Default::default(),
            max_retry_time: Default::default(),
            max_concurrent_requests: Default::default(),
            force_auth: false,
            skip_auth: false,
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
    pub fn retry_limit(mut self, retry_limit: usize) -> Self {
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
    pub fn timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = timeout.into();
        self
    }

    /// Set the read timeout duration for all HTTP requests.
    ///
    /// The timeout applies to each read operation, and resets after a
    /// successful read. This is more appropriate for detecting stalled
    /// connections when the size isn’t known beforehand.
    ///
    /// **IMPORTANT**: note this value can only be applied when the HTTP client
    /// is instantiated, it won't have any effect on a per-request basis.
    #[must_use]
    pub fn read_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.read_timeout = timeout.into();
        self
    }

    /// Set a time limit for how long a request should be retried. The default
    /// is that there isn't a limit, meaning requests are retried forever.
    ///
    /// This is a time-based variant of the [`RequestConfig::retry_limit`]
    /// method.
    #[must_use]
    pub fn max_retry_time(mut self, retry_timeout: Duration) -> Self {
        self.max_retry_time = Some(retry_timeout);
        self
    }

    /// Force sending authorization even if the endpoint does not require it.
    /// Default is only sending authorization if it is required.
    #[must_use]
    pub fn force_auth(mut self) -> Self {
        self.force_auth = true;
        self
    }

    /// Skip sending authorization headers even if the endpoint requires it.
    ///
    /// Default is to send authorization headers if the endpoint accepts or
    /// requires it.
    ///
    /// This is useful for endpoints that may optionally accept authorization
    /// but don't require it.
    ///
    /// Note: [`RequestConfig::force_auth`] takes precedence. If force auth is
    /// set, this value will be ignored.
    #[must_use]
    pub fn skip_auth(mut self) -> Self {
        self.skip_auth = true;
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
            .max_retry_time(Duration::from_secs(32))
            .retry_limit(4)
            .timeout(Duration::from_secs(600))
            .read_timeout(Duration::from_secs(10));

        assert!(cfg.force_auth);
        assert_eq!(cfg.retry_limit, Some(4));
        assert_eq!(cfg.max_retry_time, Some(Duration::from_secs(32)));
        assert_eq!(cfg.timeout, Some(Duration::from_secs(600)));
        assert_eq!(cfg.read_timeout, Some(Duration::from_secs(10)));
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
