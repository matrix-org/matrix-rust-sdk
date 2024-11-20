// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! The event cache stores holds events and downloaded media when the cache was
//! activated to save bandwidth at the cost of increased storage space usage.
//!
//! Implementing the `EventCacheStore` trait, you can plug any storage backend
//! into the event cache for the actual storage. By default this brings an
//! in-memory store.

use std::{fmt, ops::Deref, str::Utf8Error, sync::Arc};

#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;
mod memory_store;
mod traits;

use matrix_sdk_common::store_locks::{
    BackingStore, CrossProcessStoreLock, CrossProcessStoreLockGuard, LockStoreError,
};
pub use matrix_sdk_store_encryption::Error as StoreEncryptionError;

#[cfg(any(test, feature = "testing"))]
pub use self::integration_tests::EventCacheStoreIntegrationTests;
pub use self::{
    memory_store::MemoryStore,
    traits::{DynEventCacheStore, EventCacheStore, IntoEventCacheStore},
};

/// The high-level public type to represent an `EventCacheStore` lock.
#[derive(Clone)]
pub struct EventCacheStoreLock {
    /// The inner cross process lock that is used to lock the `EventCacheStore`.
    cross_process_lock: Arc<CrossProcessStoreLock<LockableEventCacheStore>>,

    /// The store itself.
    ///
    /// That's the only place where the store exists.
    store: Arc<DynEventCacheStore>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for EventCacheStoreLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("EventCacheStoreLock").finish_non_exhaustive()
    }
}

impl EventCacheStoreLock {
    /// Create a new lock around the [`EventCacheStore`].
    ///
    /// The `holder` argument represents the holder inside the
    /// [`CrossProcessStoreLock::new`].
    pub fn new<S>(store: S, holder: String) -> Self
    where
        S: IntoEventCacheStore,
    {
        let store = store.into_event_cache_store();

        Self {
            cross_process_lock: Arc::new(CrossProcessStoreLock::new(
                LockableEventCacheStore(store.clone()),
                "default".to_owned(),
                holder,
            )),
            store,
        }
    }

    /// Acquire a spin lock (see [`CrossProcessStoreLock::spin_lock`]).
    pub async fn lock(&self) -> Result<EventCacheStoreLockGuard<'_>, LockStoreError> {
        let cross_process_lock_guard = self.cross_process_lock.spin_lock(None).await?;

        Ok(EventCacheStoreLockGuard { cross_process_lock_guard, store: self.store.deref() })
    }
}

/// An RAII implementation of a “scoped lock” of an [`EventCacheStoreLock`].
/// When this structure is dropped (falls out of scope), the lock will be
/// unlocked.
pub struct EventCacheStoreLockGuard<'a> {
    /// The cross process lock guard.
    #[allow(unused)]
    cross_process_lock_guard: CrossProcessStoreLockGuard,

    /// A reference to the store.
    store: &'a DynEventCacheStore,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for EventCacheStoreLockGuard<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("EventCacheStoreLockGuard").finish_non_exhaustive()
    }
}

impl Deref for EventCacheStoreLockGuard<'_> {
    type Target = DynEventCacheStore;

    fn deref(&self) -> &Self::Target {
        self.store
    }
}

/// Event cache store specific error type.
#[derive(Debug, thiserror::Error)]
pub enum EventCacheStoreError {
    /// An error happened in the underlying database backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),

    /// The store is locked with a passphrase and an incorrect passphrase
    /// was given.
    #[error("The event cache store failed to be unlocked")]
    Locked,

    /// An unencrypted store was tried to be unlocked with a passphrase.
    #[error("The event cache store is not encrypted but tried to be opened with a passphrase")]
    Unencrypted,

    /// The store failed to encrypt or decrypt some data.
    #[error("Error encrypting or decrypting data from the event cache store: {0}")]
    Encryption(#[from] StoreEncryptionError),

    /// The store failed to encode or decode some data.
    #[error("Error encoding or decoding data from the event cache store: {0}")]
    Codec(#[from] Utf8Error),

    /// The store failed to serialize or deserialize some data.
    #[error("Error serializing or deserializing data from the event cache store: {0}")]
    Serialization(#[from] serde_json::Error),

    /// The database format has changed in a backwards incompatible way.
    #[error(
        "The database format of the event cache store changed in an incompatible way, \
         current version: {0}, latest version: {1}"
    )]
    UnsupportedDatabaseVersion(usize, usize),
}

impl EventCacheStoreError {
    /// Create a new [`Backend`][Self::Backend] error.
    ///
    /// Shorthand for `EventCacheStoreError::Backend(Box::new(error))`.
    #[inline]
    pub fn backend<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

/// An `EventCacheStore` specific result type.
pub type Result<T, E = EventCacheStoreError> = std::result::Result<T, E>;

/// A type that wraps the [`EventCacheStore`] but implements [`BackingStore`] to
/// make it usable inside the cross process lock.
#[derive(Clone, Debug)]
struct LockableEventCacheStore(Arc<DynEventCacheStore>);

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl BackingStore for LockableEventCacheStore {
    type LockError = EventCacheStoreError;

    async fn try_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> std::result::Result<bool, Self::LockError> {
        self.0.try_take_leased_lock(lease_duration_ms, key, holder).await
    }
}
