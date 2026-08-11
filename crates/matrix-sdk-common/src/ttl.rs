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

//! Types to implement TTL caches which can be used to persist data for a fixed
//! duration.

use std::{sync::Arc, time::Duration};

use ruma::time::SystemTime;
use serde::{Deserialize, Serialize};

/// A source of the current time, used so that tests can control what "now" is.
pub trait Clock: std::fmt::Debug + Send + Sync {
    /// Get the current time as a Duration since the Unix epoch.
    fn now(&self) -> Duration;
}

/// A [`Clock`] that returns the real, current system time.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("System clock was before 1970.")
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
    /// Construct a new `TtlValue` with the given data.
    pub fn new(data: T) -> Self {
        Self { data, last_fetch_ts: Some(SystemClock.now().as_secs_f64() * 1000.0) }
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

    /// Whether this value has expired, given a custom stale threshold.
    pub fn has_expired(&self, max_age: Duration, clock: Arc<dyn Clock>) -> bool {
        self.last_fetch_ts.is_some_and(|ts_ms| {
            // Convert the clock's Duration to milliseconds to match our storage format
            let now_ms = clock.now().as_secs_f64() * 1000.0;
            now_ms - ts_ms >= max_age.as_secs_f64() * 1000.0
        })
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
    pub fn into_data(self) -> T {
        self.data
    }

    /// Construct a new `TtlValue` with a specific timestamp (for testing).
    #[cfg(test)]
    pub fn new_with_timestamp(data: T, timestamp: Duration) -> Self {
        Self { data, last_fetch_ts: Some(timestamp.as_secs_f64() * 1000.0) }
    }
}

/// The default timestamp if it is missing during deserialization.
/// We expect that a value that was serialized always has an expiry time, so the
/// default is `Some(0.0)`.
fn default_timestamp() -> Option<f64> {
    Some(0.0)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::{SystemClock, TtlValue, *};

    #[test]
    fn test_ttl_value_expiry() {
        let one_day = Duration::from_secs(60 * 60 * 24);

        // Definitely stale (older than 1 day).
        let ttl_value = TtlValue {
            data: (),
            last_fetch_ts: Some(
                SystemClock.now().as_secs_f64() * 1000.0 - (one_day.as_secs_f64() * 1000.0) - 1.0,
            ),
        };
        assert!(ttl_value.has_expired(one_day, Arc::new(SystemClock)));

        // Definitely not stale.
        let ttl_value = TtlValue::new(());
        assert!(!ttl_value.has_expired(one_day, Arc::new(SystemClock)));

        // Cannot be stale.
        let ttl_value = TtlValue::without_expiry(());
        assert!(!ttl_value.has_expired(one_day, Arc::new(SystemClock)));
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
