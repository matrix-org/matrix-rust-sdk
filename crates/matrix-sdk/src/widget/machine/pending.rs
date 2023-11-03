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

//! A wrapper around a hash map that tracks pending requests and makes sure
//! that expired requests are removed.

use std::time::{Duration, Instant};

use indexmap::{map::Entry, IndexMap};
use tracing::warn;
use uuid::Uuid;

/// Configuration of limits for the outgoing request handling.
#[derive(Clone, Debug)]
pub(crate) struct RequestLimits {
    /// Maximum amount of unanswered (pending) requests that the client widget
    /// API is going to process before starting to drop them. This ensures
    /// that a buggy widget cannot force the client machine to consume memory
    /// indefinitely.
    pub(crate) max_pending_requests: usize,
    /// For how long can the unanswered (pending) request stored in a map before
    /// it is dropped. This ensures that requests that are not answered within
    /// a ceratin amount of time, are dropped/cleaned up (considered as failed).
    pub(crate) response_timeout: Duration,
}

/// A wrapper around a hash map that ensures that the request limits
/// are taken into account.
///
/// Expired requests get cleaned up so that the hashmap remains
/// limited to a certain amount of pending requests.
pub(super) struct PendingRequests<T> {
    requests: IndexMap<Uuid, Expirable<T>>,
    limits: RequestLimits,
}

impl<T> PendingRequests<T> {
    pub(super) fn new(limits: RequestLimits) -> Self {
        Self { requests: IndexMap::with_capacity(limits.max_pending_requests), limits }
    }

    /// Inserts a new request into the map.
    ///
    /// Returns `None` if the maximum allowed capacity is reached.
    pub(super) fn insert(&mut self, key: Uuid, value: T) -> Option<&mut T> {
        if self.requests.len() >= self.limits.max_pending_requests {
            return None;
        }

        let Entry::Vacant(entry) = self.requests.entry(key) else {
            panic!("uuid collision");
        };

        let expirable = Expirable::new(value, Instant::now() + self.limits.response_timeout);
        let inserted = entry.insert(expirable);
        Some(&mut inserted.value)
    }

    /// Extracts a request from the map based on its identifier.
    ///
    /// Returns `None` if the value is not present or expired.
    pub(super) fn extract(&mut self, key: &Uuid) -> Result<T, &'static str> {
        let value = self.requests.remove(key).ok_or("Received response for an unknown request")?;
        value.value().ok_or("Dropping response for an expired request")
    }

    /// Removes all expired requests from the map.
    pub(super) fn remove_expired(&mut self) {
        self.requests.retain(|id, req| {
            let expired = req.expired();
            if expired {
                warn!(?id, "Dropping response for an expired request");
            }
            !expired
        });
    }
}

struct Expirable<T> {
    value: T,
    expires_at: Instant,
}

impl<T> Expirable<T> {
    fn new(value: T, expires_at: Instant) -> Self {
        Self { value, expires_at }
    }

    fn value(self) -> Option<T> {
        (!self.expired()).then_some(self.value)
    }

    fn expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use uuid::Uuid;

    use super::{PendingRequests, RequestLimits};

    struct Dummy;

    #[test]
    fn insertion_limits_for_pending_requests_work() {
        let mut pending: PendingRequests<Dummy> = PendingRequests::new(RequestLimits {
            max_pending_requests: 1,
            response_timeout: Duration::from_secs(10),
        });

        // First insert is ok.
        let first = Uuid::new_v4();
        assert!(pending.insert(first, Dummy).is_some());
        assert!(!pending.requests.is_empty());

        // Second insert fails - limits is 1 pending request.
        let second = Uuid::new_v4();
        assert!(pending.insert(second, Dummy).is_none());

        // First extract is ok.
        // Second extract fails - it's not in a map.
        assert!(pending.extract(&first).is_ok());
        assert!(pending.extract(&second).is_err());

        // After first is extracted, we have capacity for the second one.
        assert!(pending.insert(second, Dummy).is_some());
        // So extracting it should also work.
        assert!(pending.extract(&second).is_ok());
        // After extraction, we expect that the map is empty.
        assert!(pending.requests.is_empty());
    }

    #[test]
    fn time_limits_for_pending_requests_work() {
        let mut pending: PendingRequests<Dummy> = PendingRequests::new(RequestLimits {
            max_pending_requests: 10,
            response_timeout: Duration::from_secs(1),
        });

        // Insert a request, it's fine, limits are high.
        let key = Uuid::new_v4();
        assert!(pending.insert(key, Dummy).is_some());

        // Wait for 2 seconds, the inserted request should lapse.
        std::thread::sleep(Duration::from_secs(2));
        assert!(pending.extract(&key).is_err());

        // Insert 2 requests. Should be fine, limits are high.
        assert!(pending.insert(Uuid::new_v4(), Dummy).is_some());
        assert!(pending.insert(Uuid::new_v4(), Dummy).is_some());

        // Wait for half a second, remove expired ones (none must be removed).
        // Then, add another one (should also be fine, limits are high). So
        // we should have 3 requests in a hash map.
        std::thread::sleep(Duration::from_millis(500));
        pending.remove_expired();
        let key = Uuid::new_v4();
        assert!(pending.insert(key, Dummy).is_some());
        assert!(pending.requests.len() == 3);

        // Wait for another half a second. First two requests should lapse.
        // But the last one should still be in the map.
        std::thread::sleep(Duration::from_millis(500));
        pending.remove_expired();
        assert!(pending.requests.len() == 1);
        assert!(pending.extract(&key).is_ok());
        assert!(pending.requests.is_empty());
    }
}
