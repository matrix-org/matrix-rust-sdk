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

//! A TTL cache which can be used to time out repeated operations that might
//! experience intermittent failures.

use std::{borrow::Borrow, collections::HashMap, hash::Hash, sync::Arc, time::Duration};

use ruma::time::Instant;

use super::locks::RwLock;

const MAX_DELAY: u64 = 15 * 60;
const MULTIPLIER: u64 = 15;

/// A TTL cache where items get inactive instead of discarded.
///
/// The items need to be explicitly removed from the cache. This allows us to
/// implement exponential backoff based TTL.
#[derive(Clone, Debug)]
pub struct FailuresCache<T: Eq + Hash> {
    inner: Arc<InnerCache<T>>,
}

#[derive(Debug)]
struct InnerCache<T: Eq + Hash> {
    max_delay: Duration,
    backoff_multiplier: u64,
    items: RwLock<HashMap<T, FailuresItem>>,
}

impl<T: Eq + Hash> Default for InnerCache<T> {
    fn default() -> Self {
        Self {
            max_delay: Duration::from_secs(MAX_DELAY),
            backoff_multiplier: MULTIPLIER,
            items: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FailuresItem {
    insertion_time: Instant,
    duration: Duration,

    /// Number of times that this item has failed after it was first added to
    /// the cache. (In other words, one less than the total number of
    /// failures.)
    failure_count: u8,
}

impl FailuresItem {
    /// Has the item expired.
    fn expired(&self) -> bool {
        self.insertion_time.elapsed() >= self.duration
    }

    /// Force the expiry of this item.
    ///
    /// This doesn't reset the failure count, but does mark the item as ready
    /// for immediate retry.
    fn expire(&mut self) {
        self.duration = Duration::from_secs(0);
    }
}

impl<T> FailuresCache<T>
where
    T: Eq + Hash,
{
    pub fn new() -> Self {
        Self { inner: Default::default() }
    }

    pub fn with_settings(max_delay: Duration, multiplier: u8) -> Self {
        Self {
            inner: InnerCache {
                max_delay,
                backoff_multiplier: multiplier.into(),
                items: Default::default(),
            }
            .into(),
        }
    }

    /// Is the given key non-expired and part of the cache.
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.items.read().get(key).is_some_and(|item| !item.expired())
    }

    /// Get the failure count for a given key.
    ///
    /// # Returns
    ///
    ///  * `None` if this key is not in the failure cache. (It has never failed,
    ///    or it has been [`FailuresCache::remove()`]d since the last failure.)
    ///
    ///  * `Some(u8)`: the number of times it has failed since it was first
    ///    added to the failure cache. (In other words, one less than the total
    ///    number of failures.)
    pub fn failure_count<Q>(&self, key: &Q) -> Option<u8>
    where
        T: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.inner.items.read().get(key).map(|i| i.failure_count)
    }

    /// This will calculate a duration that determines how long an item is
    /// considered to be valid while being in the cache.
    ///
    /// The returned duration will follow this sequence if the default
    /// multiplier and `max_delay` values are used, values are in minutes:
    ///      [0.25, 0.5, 1.0, 2.0, 4.0, 8.0, 15.0]
    fn calculate_delay(&self, failure_count: u8) -> Duration {
        let exponential_backoff = 2u64.saturating_pow(failure_count.into());
        let delay = exponential_backoff.saturating_mul(self.inner.backoff_multiplier);

        Duration::from_secs(delay).clamp(Duration::from_secs(1), self.inner.max_delay)
    }

    /// Add a single item to the cache.
    pub fn insert(&self, item: T) {
        self.extend([item]);
    }

    /// Extend the cache with the given iterator of items.
    ///
    /// Items that are already part of the cache, whether they are expired or
    /// not, will have their TTL extended using an exponential backoff
    /// algorithm.
    pub fn extend(&self, iterator: impl IntoIterator<Item = T>) {
        let mut lock = self.inner.items.write();

        let now = Instant::now();

        for key in iterator {
            let failure_count = if let Some(value) = lock.get(&key) {
                value.failure_count.saturating_add(1)
            } else {
                0
            };

            let delay = self.calculate_delay(failure_count);

            let item = FailuresItem { insertion_time: now, duration: delay, failure_count };

            lock.insert(key, item);
        }
    }

    /// Remove the items contained in the iterator from the cache.
    pub fn remove<'a, I, Q>(&'a self, iterator: I)
    where
        I: Iterator<Item = &'a Q>,
        T: Borrow<Q>,
        Q: Hash + Eq + 'a + ?Sized,
    {
        let mut lock = self.inner.items.write();

        for item in iterator {
            lock.remove(item);
        }
    }

    /// Force the expiry of the given item, if it is present in the cache.
    ///
    /// This doesn't reset the failure count, but does mark the item as ready
    /// for immediate retry.
    #[doc(hidden)]
    pub fn expire(&self, item: &T) {
        self.inner.items.write().get_mut(item).map(FailuresItem::expire);
    }
}

impl<T: Eq + Hash> Default for FailuresCache<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use proptest::prelude::*;

    use super::FailuresCache;

    #[test]
    fn failures_cache() {
        let cache = FailuresCache::new();

        assert!(!cache.contains(&1));
        cache.extend([1u8].iter());
        assert!(cache.contains(&1));

        cache.inner.items.write().get_mut(&1).unwrap().duration = Duration::from_secs(0);
        assert!(!cache.contains(&1));

        cache.remove([1u8].iter());
        assert!(cache.inner.items.read().get(&1).is_none())
    }

    #[test]
    fn failures_cache_timeout() {
        let cache: FailuresCache<u8> = FailuresCache::new();

        assert_eq!(cache.calculate_delay(0).as_secs(), 15);
        assert_eq!(cache.calculate_delay(1).as_secs(), 30);
        assert_eq!(cache.calculate_delay(2).as_secs(), 60);
        assert_eq!(cache.calculate_delay(3).as_secs(), 120);
        assert_eq!(cache.calculate_delay(4).as_secs(), 240);
        assert_eq!(cache.calculate_delay(5).as_secs(), 480);
        assert_eq!(cache.calculate_delay(6).as_secs(), 900);
        assert_eq!(cache.calculate_delay(7).as_secs(), 900);
    }

    proptest! {
        #[test]
        fn failures_cache_proptest_timeout(count in 0..10u8) {
            let cache: FailuresCache<u8> = FailuresCache::new();
            let delay = cache.calculate_delay(count).as_secs();

            assert!(delay <= 900);
            assert!(delay >= 15);
        }
    }
}
