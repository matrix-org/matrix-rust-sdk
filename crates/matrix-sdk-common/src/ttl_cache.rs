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

use ruma::time::{Instant, SystemTime};
use serde::{Deserialize, Serialize};

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

/// A value that expires after some time.
///
/// This value is (de)serializable so it can be persisted in a store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtlValue<T> {
    /// The data of the item.
    #[serde(flatten)]
    data: T,

    /// Last time we fetched this data from the server, in milliseconds since
    /// UNIX epoch.
    ///
    /// When this field is missing during deserialization, it defaults to `0.0`,
    /// which means that the data is always expired. This allows to be
    /// compatible with data that was persisted before deciding to add an
    /// expiration time.
    #[serde(default = "default_timestamp")]
    last_fetch_ts: Option<f64>,
}

impl<T> TtlValue<T> {
    /// The number of milliseconds after which the data is considered stale.
    ///
    /// This matches 7 days.
    pub const STALE_THRESHOLD: f64 = (1000 * 60 * 60 * 24) as _;

    /// Construct a new `TtlValue` with the given data.
    pub fn new(data: T) -> Self {
        Self { data, last_fetch_ts: Some(now_timestamp_ms()) }
    }

    /// Construct a new `TtlValue` with the given data that never expires.
    pub fn without_expiry(data: T) -> Self {
        Self { data, last_fetch_ts: None }
    }

    /// Converts from `&TtlValue<T>` to `TtlValue<&T>`.
    pub fn as_ref(&self) -> TtlValue<&T> {
        TtlValue { data: &self.data, last_fetch_ts: self.last_fetch_ts }
    }

    /// Transform the data of this `TtlValue` with the given function.
    pub fn map<U, F>(self, f: F) -> TtlValue<U>
    where
        F: FnOnce(T) -> U,
    {
        TtlValue { data: f(self.data), last_fetch_ts: self.last_fetch_ts }
    }

    /// Whether this value has expired.
    pub fn has_expired(&self) -> bool {
        self.last_fetch_ts.is_some_and(|ts| now_timestamp_ms() - ts >= Self::STALE_THRESHOLD)
    }

    /// Mark this value has expired.
    pub fn expire(&mut self) {
        // We assume that the system time is always correct and we are far from the UNIX
        // epoch so a timestamp of 0 should always be expired.
        self.last_fetch_ts = Some(0.0)
    }

    /// Get a reference to the data of this value.
    pub fn data(&self) -> &T {
        &self.data
    }

    /// Get the data of this value.
    pub fn into_data_unchecked(self) -> T {
        self.data
    }

    /// Get the data of this value if it hasn't expired.
    pub fn into_data(self) -> Option<T> {
        (!self.has_expired()).then_some(self.data)
    }
}

/// Get the current timestamp as the number of milliseconds since Unix Epoch.
fn now_timestamp_ms() -> f64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("System clock was before 1970.")
        .as_secs_f64()
        * 1000.0
}

/// The default timestamp if it is missing during deserialization.
///
/// We expect that a value that was serialized always has an expiry time, so the
/// default is `Some(0.0)`.
fn default_timestamp() -> Option<f64> {
    Some(0.0)
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::{TtlCache, TtlValue, now_timestamp_ms};

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

    #[test]
    fn test_ttl_value_expiry() {
        // Definitely stale.
        let ttl_value = TtlValue {
            data: (),
            last_fetch_ts: Some(now_timestamp_ms() - TtlValue::<()>::STALE_THRESHOLD - 1.0),
        };
        assert!(ttl_value.has_expired());

        // Definitely not stale.
        let ttl_value = TtlValue::new(());
        assert!(!ttl_value.has_expired());

        // Cannot be stale.
        let ttl_value = TtlValue::without_expiry(());
        assert!(!ttl_value.has_expired());
    }

    #[test]
    fn test_ttl_value_serialize_roundtrip() {
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        struct Data {
            foo: String,
        }

        let data = Data { foo: "bar".to_owned() };

        // With timestamp.
        let ttl_value = TtlValue { data: data.clone(), last_fetch_ts: Some(1000.0) };
        let json = json!({
            "foo": "bar",
            "last_fetch_ts": 1000.0,
        });
        assert_eq!(serde_json::to_value(&ttl_value).unwrap(), json);

        let deserialized = serde_json::from_value::<TtlValue<Data>>(json).unwrap();
        assert_eq!(deserialized.data, data);
        assert!(deserialized.last_fetch_ts.unwrap() - ttl_value.last_fetch_ts.unwrap() < 0.0001);

        // Without timestamp the value is always expired in theory.
        let json = json!({
            "foo": "bar",
        });
        let deserialized = serde_json::from_value::<TtlValue<Data>>(json).unwrap();
        assert_eq!(deserialized.data, data);
        assert!(deserialized.last_fetch_ts.unwrap() - 0.0 < 0.0001);
    }
}
