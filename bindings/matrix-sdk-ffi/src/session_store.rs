#[cfg(feature = "sqlite")]
use std::path::PathBuf;

#[cfg(feature = "sqlite")]
use matrix_sdk::SqliteStoreConfig;

/// The result of building a [`SessionStoreConfig`], with data that
/// can be passed directly to a ClientBuilder.
pub enum SessionStoreResult {
    #[cfg(feature = "sqlite")]
    Sqlite { config: SqliteStoreConfig, cache_path: PathBuf, store_path: PathBuf },
    #[cfg(feature = "indexeddb")]
    IndexedDb { name: String, passphrase: Option<String> },
}

#[cfg(feature = "sqlite")]
mod sqlite_session_store {
    use std::{fs, path::Path, sync::Arc};

    use matrix_sdk::SqliteStoreConfig;
    use tracing::debug;
    use zeroize::Zeroizing;

    use super::SessionStoreResult;
    use crate::{client_builder::ClientBuildError, helpers::unwrap_or_clone_arc};

    /// The store paths the client will use when built.
    #[derive(Clone)]
    struct SessionPaths {
        /// The path that the client will use to store its data.
        data_path: String,
        /// The path that the client will use to store its caches. This path can
        /// be the same as the data path if you prefer to keep
        /// everything in one place.
        cache_path: String,
    }

    /// A builder for configuring a Sqlite session store.
    #[derive(Clone, uniffi::Object)]
    pub struct SqliteSessionStoreBuilder {
        paths: SessionPaths,
        passphrase: Zeroizing<Option<String>>,
        pool_max_size: Option<usize>,
        cache_size: Option<u32>,
        journal_size_limit: Option<u32>,
        system_is_memory_constrained: bool,
    }

    #[matrix_sdk_ffi_macros::export]
    impl SqliteSessionStoreBuilder {
        /// Construct a SqliteSessionStoreBuilder and set the paths that the
        /// client will use to store its data and caches.
        ///
        /// Both paths **must** be unique per session as the SDK stores aren't
        /// capable of handling multiple users, however it is valid to use the
        /// same path for both stores on a single session.
        #[uniffi::constructor]
        pub fn new(data_path: String, cache_path: String) -> Arc<Self> {
            Arc::new(Self {
                paths: SessionPaths { data_path, cache_path },
                passphrase: Zeroizing::new(None),
                pool_max_size: None,
                cache_size: None,
                journal_size_limit: None,
                system_is_memory_constrained: false,
            })
        }

        /// Set the passphrase for the stores given to
        /// [`ClientBuilder::paths`].
        pub fn passphrase(self: Arc<Self>, passphrase: Option<String>) -> Arc<Self> {
            let mut builder = unwrap_or_clone_arc(self);
            builder.passphrase = Zeroizing::new(passphrase);
            Arc::new(builder)
        }

        /// Set the pool max size for the SQLite stores given to
        /// [`ClientBuilder::session_paths`].
        ///
        /// Each store exposes an async pool of connections. This method
        /// controls the size of the pool. The larger the pool is, the
        /// more memory is consumed, but also the more the app is
        /// reactive because it doesn't need to wait on a pool to be
        /// available to run queries.
        ///
        /// See [`SqliteStoreConfig::pool_max_size`] to learn more.
        pub fn pool_max_size(self: Arc<Self>, pool_max_size: Option<u32>) -> Arc<Self> {
            let mut builder = unwrap_or_clone_arc(self);
            builder.pool_max_size = pool_max_size.map(|size| {
                size.try_into().expect("`pool_max_size` is too large to fit in `usize`")
            });
            Arc::new(builder)
        }

        /// Set the cache size for the SQLite stores given to
        /// [`ClientBuilder::session_paths`].
        ///
        /// Each store exposes a SQLite connection. This method controls the
        /// cache size, in **bytes (!)**.
        ///
        /// The cache represents data SQLite holds in memory at once per open
        /// database file. The default cache implementation does not allocate
        /// the full amount of cache memory all at once. Cache memory is
        /// allocated in smaller chunks on an as-needed basis.
        ///
        /// See [`SqliteStoreConfig::cache_size`] to learn more.
        pub fn cache_size(self: Arc<Self>, cache_size: Option<u32>) -> Arc<Self> {
            let mut builder = unwrap_or_clone_arc(self);
            builder.cache_size = cache_size;
            Arc::new(builder)
        }

        /// Set the size limit for the SQLite WAL files of stores given to
        /// [`ClientBuilder::session_paths`].
        ///
        /// Each store uses the WAL journal mode. This method controls the size
        /// limit of the WAL files, in **bytes (!)**.
        ///
        /// See [`SqliteStoreConfig::journal_size_limit`] to learn more.
        pub fn journal_size_limit(self: Arc<Self>, limit: Option<u32>) -> Arc<Self> {
            let mut builder = unwrap_or_clone_arc(self);
            builder.journal_size_limit = limit;
            Arc::new(builder)
        }

        /// Tell the client that the system is memory constrained, like in a
        /// push notification process for example.
        ///
        /// So far, at the time of writing (2025-04-07), it changes the defaults
        /// of [`SqliteStoreConfig`], so one might not need to call
        /// [`ClientBuilder::session_cache_size`] and siblings for example.
        /// Please check [`SqliteStoreConfig::with_low_memory_config`].
        pub fn system_is_memory_constrained(self: Arc<Self>) -> Arc<Self> {
            let mut builder = unwrap_or_clone_arc(self);
            builder.system_is_memory_constrained = true;
            Arc::new(builder)
        }
    }

    impl SqliteSessionStoreBuilder {
        pub fn build(&self) -> Result<SessionStoreResult, ClientBuildError> {
            let data_path = Path::new(&self.paths.data_path);
            let cache_path = Path::new(&self.paths.cache_path);

            debug!(
                data_path = %data_path.to_string_lossy(),
                cache_path = %cache_path.to_string_lossy(),
                "Creating directories for data and cache stores.",
            );

            fs::create_dir_all(data_path)?;
            fs::create_dir_all(cache_path)?;

            let mut sqlite_store_config = if self.system_is_memory_constrained {
                SqliteStoreConfig::with_low_memory_config(data_path)
            } else {
                SqliteStoreConfig::new(data_path)
            };

            sqlite_store_config = sqlite_store_config.passphrase(self.passphrase.as_deref());

            if let Some(size) = self.pool_max_size {
                sqlite_store_config = sqlite_store_config.pool_max_size(size);
            }

            if let Some(size) = self.cache_size {
                sqlite_store_config = sqlite_store_config.cache_size(size);
            }

            if let Some(limit) = self.journal_size_limit {
                sqlite_store_config = sqlite_store_config.journal_size_limit(limit);
            }

            Ok(SessionStoreResult::Sqlite {
                config: sqlite_store_config,
                store_path: data_path.to_owned(),
                cache_path: cache_path.to_owned(),
            })
        }
    }
}

#[cfg(feature = "indexeddb")]
mod indexeddb_session_store {
    use std::sync::Arc;

    use super::SessionStoreResult;
    use crate::{client_builder::ClientBuildError, helpers::unwrap_or_clone_arc};

    #[derive(Clone, uniffi::Object)]
    pub struct IndexedDbSessionStoreBuilder {
        name: String,
        passphrase: Option<String>,
    }

    #[matrix_sdk_ffi_macros::export]
    impl IndexedDbSessionStoreBuilder {
        #[uniffi::constructor]
        pub fn new(name: String) -> Arc<Self> {
            Arc::new(Self { name, passphrase: None })
        }

        /// Set the passphrase for the IndexedDB store.
        pub fn passphrase(self: Arc<Self>, passphrase: Option<String>) -> Arc<Self> {
            let mut builder = unwrap_or_clone_arc(self);
            builder.passphrase = passphrase;
            Arc::new(builder)
        }
    }

    impl IndexedDbSessionStoreBuilder {
        pub fn build(&self) -> Result<SessionStoreResult, ClientBuildError> {
            Ok(SessionStoreResult::IndexedDb {
                name: self.name.clone(),
                passphrase: self.passphrase.clone(),
            })
        }
    }
}

#[cfg(feature = "indexeddb")]
pub use indexeddb_session_store::*;
#[cfg(feature = "sqlite")]
pub use sqlite_session_store::*;

use crate::client_builder::ClientBuildError;

/// Internal struct for tracking the choice of session store config.
#[derive(Clone)]
pub enum SessionStoreConfig {
    #[cfg(feature = "sqlite")]
    /// Setup the client to use the SQLite store.
    Sqlite(SqliteSessionStoreBuilder),

    #[cfg(feature = "indexeddb")]
    /// Setup the client to use the IndexedDB store.
    IndexedDb(IndexedDbSessionStoreBuilder),
}

impl SessionStoreConfig {
    pub(crate) fn build(&self) -> Result<SessionStoreResult, ClientBuildError> {
        match self {
            #[cfg(feature = "sqlite")]
            SessionStoreConfig::Sqlite(config) => config.build(),

            #[cfg(feature = "indexeddb")]
            SessionStoreConfig::IndexedDb(config) => config.build(),
        }
    }
}
