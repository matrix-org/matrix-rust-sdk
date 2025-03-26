// Copyright 2022 The Matrix.org Foundation C.I.C.
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
#![cfg_attr(
    not(any(feature = "state-store", feature = "crypto-store", feature = "event-cache")),
    allow(dead_code, unused_imports)
)]

#[cfg(feature = "crypto-store")]
mod crypto_store;
mod error;
#[cfg(feature = "event-cache")]
mod event_cache_store;
#[cfg(feature = "state-store")]
mod state_store;
mod utils;
use std::path::{Path, PathBuf};

use deadpool_sqlite::PoolConfig;

#[cfg(feature = "crypto-store")]
pub use self::crypto_store::SqliteCryptoStore;
pub use self::error::OpenStoreError;
#[cfg(feature = "event-cache")]
pub use self::event_cache_store::SqliteEventCacheStore;
#[cfg(feature = "state-store")]
pub use self::state_store::SqliteStateStore;

#[cfg(test)]
matrix_sdk_test::init_tracing_for_tests!();

/// A configuration structure used for opening a store.
pub struct SqliteStoreConfig {
    /// Path to the database, without the file name.
    path: PathBuf,
    /// Passphrase to open the store, if any.
    passphrase: Option<String>,
    /// The pool configuration for [`deadpool_sqlite`].
    pool_config: PoolConfig,
}

impl SqliteStoreConfig {
    /// Create a new [`SqliteStoreConfig`] with a path representing the
    /// directory containing the store database.
    pub fn new<P>(path: P) -> Self
    where
        P: AsRef<Path>,
    {
        Self {
            path: path.as_ref().to_path_buf(),
            passphrase: None,
            pool_config: PoolConfig::new(num_cpus::get_physical() * 4),
        }
    }

    /// Define the passphrase if the store is encoded.
    pub fn passphrase(mut self, passphrase: Option<&str>) -> Self {
        self.passphrase = passphrase.map(|passphrase| passphrase.to_owned());
        self
    }

    /// Define the maximum pool size for [`deadpool_sqlite`].
    ///
    /// See [`deadpool_sqlite::PoolConfig::max_size`] to learn more.
    pub fn pool_max_size(mut self, max_size: usize) -> Self {
        self.pool_config.max_size = max_size;
        self
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::SqliteStoreConfig;

    #[test]
    fn test_store_open_config() {
        let store_open_config =
            SqliteStoreConfig::new(Path::new("foo")).passphrase(Some("bar")).pool_max_size(42);

        assert_eq!(store_open_config.path, PathBuf::from("foo"));
        assert_eq!(store_open_config.passphrase, Some("bar".to_owned()));
        assert_eq!(store_open_config.pool_config.max_size, 42);
    }
}
