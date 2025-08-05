// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::{borrow::Borrow, collections::HashMap, hash::Hash, time::Duration};

use ruma::time::Instant;

// One day is the default lifetime.
const DEFAULT_LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug)]
struct TtlItem<V: Clone> {
    value: V,
    insertion_time: Instant,
    lifetime: Duration,
}

impl<V: Clone> TtlItem<V> {
    fn expired(&self) -> bool {
        self.insertion_time.elapsed() >= self.lifetime
    }
}

/// A TTL cache where items get removed deterministically in the `get()` call.
#[derive(Debug)]
pub struct TtlCache<K: Eq + Hash, V: Clone> {
    lifetime: Duration,
    items: HashMap<K, TtlItem<V>>,
}

impl<K, V> TtlCache<K, V>
where
    K: Eq + Hash,
    V: Clone,
{
    /// Create a new, empty, [`TtlCache`].
    pub fn new() -> Self {
        Self { items: Default::default(), lifetime: DEFAULT_LIFETIME }
    }

    /// Does the cache contain an non-expired item with the matching key.
    pub fn contains<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let cache = &self.items;
        if let Some(item) = cache.get(key) { !item.expired() } else { false }
    }

    /// Add a single item to the cache.
    pub fn insert(&mut self, key: K, value: V) {
        self.extend([(key, value)]);
    }

    /// Extend the cache with the given iterator of items.
    pub fn extend(&mut self, iterator: impl IntoIterator<Item = (K, V)>) {
        let cache = &mut self.items;

        let now = Instant::now();

        for (key, value) in iterator {
            let item = TtlItem { value, insertion_time: now, lifetime: self.lifetime };

            cache.insert(key, item);
        }
    }

    /// Remove the item that matches the given key.
    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.items.remove(key.borrow()).map(|item| item.value)
    }

    /// Get the item that matches the given key, if the item has expired `None`
    /// will be returned and the item will be evicted from the cache.
    pub fn get<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        // Remove all expired items.
        self.items.retain(|_, value| !value.expired());
        // Now get the wanted item.
        self.items.get(key.borrow()).map(|item| item.value.clone())
    }

    /// Force the expiry of the given item, if it is present in the cache.
    ///
    /// This doesn't remove the item, it just marks it as expired.
    #[doc(hidden)]
    pub fn expire<Q>(&mut self, key: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Some(item) = self.items.get_mut(key) {
            item.lifetime = Duration::from_secs(0);
        }
    }
}

impl<K: Eq + Hash, V: Clone> Default for TtlCache<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {

    use super::TtlCache;

    #[test]
    fn test_ttl_cache_insertion() {
        let mut cache = TtlCache::new();
        assert!(!cache.contains("A"));

        cache.insert("A", 1);
        assert!(cache.contains("A"));

        let value = cache.get("A").expect("The value should be in the cache");
        assert_eq!(value, 1);

        cache.expire("A");

        assert!(!cache.contains("A"));
        assert!(cache.get("A").is_none(), "The item should have been removed from the cache");
    }
}
