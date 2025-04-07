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
use std::{
    fmt,
    path::{Path, PathBuf},
};

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
#[derive(Clone)]
pub struct SqliteStoreConfig {
    /// Path to the database, without the file name.
    path: PathBuf,
    /// Passphrase to open the store, if any.
    passphrase: Option<String>,
    /// The pool configuration for [`deadpool_sqlite`].
    pool_config: PoolConfig,
    /// The runtime configuration to apply when opening an SQLite connection.
    runtime_config: RuntimeConfig,
}

impl fmt::Debug for SqliteStoreConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SqliteStoreConfig")
            .field("path", &self.path)
            .field("pool_config", &self.pool_config)
            .field("runtime_config", &self.runtime_config)
            .finish_non_exhaustive()
    }
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
            runtime_config: RuntimeConfig::default(),
        }
    }

    /// Similar to [`SqliteStoreConfig::new`], but with defaults tailored for a
    /// low memory usage environment.
    ///
    /// The following defaults are set:
    ///
    /// * The `pool_max_size` is set to the number of physical CPU, so one
    ///   connection per physical thread,
    /// * The `cache_size` is set to 500Kib,
    /// * The `journal_size_limit` is set to 2Mib.
    pub fn with_low_memory_config<P>(path: P) -> Self
    where
        P: AsRef<Path>,
    {
        Self::new(path)
            // Maximum one connection per physical thread.
            .pool_max_size(num_cpus::get_physical())
            // Cache size is 500Kib.
            .cache_size(500_000)
            // Journal size limit is 2Mib.
            .journal_size_limit(2_000_000)
    }

    /// Override the path.
    pub fn path<P>(mut self, path: P) -> Self
    where
        P: AsRef<Path>,
    {
        self.path = path.as_ref().to_path_buf();
        self
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

    /// Optimize the database.
    ///
    /// The SQLite documentation recommends to run this regularly and after any
    /// schema change. The easiest is to do it consistently when the store is
    /// constructed, after eventual migrations.
    ///
    /// See [`PRAGMA optimize`] to learn more.
    ///
    /// The default value is `true`.
    ///
    /// [`PRAGMA optimize`]: https://www.sqlite.org/pragma.html#pragma_optimize
    pub fn optimize(mut self, optimize: bool) -> Self {
        self.runtime_config.optimize = optimize;
        self
    }

    /// Define the maximum size in **bytes** the SQLite cache can use.
    ///
    /// See [`PRAGMA cache_size`] to learn more.
    ///
    /// The default value is 2Mib.
    ///
    /// [`PRAGMA cache_size`]: https://www.sqlite.org/pragma.html#pragma_cache_size
    pub fn cache_size(mut self, cache_size: u32) -> Self {
        self.runtime_config.cache_size = cache_size;
        self
    }

    /// Limit the size of the WAL file, in **bytes**.
    ///
    /// By default, while the DB connections of the databases are open, [the
    /// size of the WAL file can keep increasing][size_wal_file] depending on
    /// the size needed for the transactions. A critical case is `VACUUM`
    /// which basically writes the content of the DB file to the WAL file
    /// before writing it back to the DB file, so we end up taking twice the
    /// size of the database.
    ///
    /// By setting this limit, the WAL file is truncated after its content is
    /// written to the database, if it is bigger than the limit.
    ///
    /// See [`PRAGMA journal_size_limit`] to learn more. The value `limit`
    /// corresponds to `N` in `PRAGMA journal_size_limit = N`.
    ///
    /// The default value is 10Mib.
    ///
    /// [size_wal_file]: https://www.sqlite.org/wal.html#avoiding_excessively_large_wal_files
    /// [`PRAGMA journal_size_limit`]: https://www.sqlite.org/pragma.html#pragma_journal_size_limit
    pub fn journal_size_limit(mut self, limit: u32) -> Self {
        self.runtime_config.journal_size_limit = limit;
        self
    }
}

/// This type represents values to set at runtime when a database is opened.
///
/// This configuration is applied by
/// [`utils::SqliteAsyncConnExt::apply_runtime_config`].
#[derive(Clone, Debug)]
struct RuntimeConfig {
    /// If `true`, [`utils::SqliteAsyncConnExt::optimize`] will be called.
    optimize: bool,

    /// Regardless of the value, [`utils::SqliteAsyncConnExt::cache_size`] will
    /// always be called with this value.
    cache_size: u32,

    /// Regardless of the value,
    /// [`utils::SqliteAsyncConnExt::journal_size_limit`] will always be called
    /// with this value.
    journal_size_limit: u32,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            // Optimize is always applied.
            optimize: true,
            // A cache of 2Mib.
            cache_size: 2_000_000,
            // A limit of 10Mib.
            journal_size_limit: 10_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ops::Not,
        path::{Path, PathBuf},
    };

    use super::SqliteStoreConfig;

    #[test]
    fn test_new() {
        let store_config = SqliteStoreConfig::new(Path::new("foo"));

        assert_eq!(store_config.pool_config.max_size, num_cpus::get_physical() * 4);
        assert!(store_config.runtime_config.optimize);
        assert_eq!(store_config.runtime_config.cache_size, 2_000_000);
        assert_eq!(store_config.runtime_config.journal_size_limit, 10_000_000);
    }

    #[test]
    fn test_with_low_memory_config() {
        let store_config = SqliteStoreConfig::with_low_memory_config(Path::new("foo"));

        assert_eq!(store_config.pool_config.max_size, num_cpus::get_physical());
        assert!(store_config.runtime_config.optimize);
        assert_eq!(store_config.runtime_config.cache_size, 500_000);
        assert_eq!(store_config.runtime_config.journal_size_limit, 2_000_000);
    }

    #[test]
    fn test_store_config() {
        let store_config = SqliteStoreConfig::new(Path::new("foo"))
            .passphrase(Some("bar"))
            .pool_max_size(42)
            .optimize(false)
            .cache_size(43)
            .journal_size_limit(44);

        assert_eq!(store_config.path, PathBuf::from("foo"));
        assert_eq!(store_config.passphrase, Some("bar".to_owned()));
        assert_eq!(store_config.pool_config.max_size, 42);
        assert!(store_config.runtime_config.optimize.not());
        assert_eq!(store_config.runtime_config.cache_size, 43);
        assert_eq!(store_config.runtime_config.journal_size_limit, 44);
    }

    #[test]
    fn test_store_config_path() {
        let store_config = SqliteStoreConfig::new(Path::new("foo")).path(Path::new("bar"));

        assert_eq!(store_config.path, PathBuf::from("bar"));
    }
}
