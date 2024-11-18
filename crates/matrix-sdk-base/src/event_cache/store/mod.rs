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

use std::{
    error::Error,
    fmt,
    ops::{Deref, DerefMut},
    result::Result as StdResult,
    str::Utf8Error,
    sync::{Arc, Mutex},
};

#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;
mod memory_store;
mod traits;

use matrix_sdk_common::store_locks::{
    BackingStore, CrossProcessStoreLock, CrossProcessStoreLockGuard, LockGeneration,
    LockStoreError, FIRST_LOCK_GENERATION,
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
    cross_process_lock: CrossProcessStoreLock<Arc<LockableEventCacheStore>>,

    /// A reference to the `LockableEventCacheStore`.
    ///
    /// This is used to get access to extra API on `LockableEventCacheStore`,
    /// not restricted to the `BackingStore` trait.
    ///
    /// This is necessary because `CrossProcessStoreLock` doesn't provide a way
    /// to get a reference to the inner backing store. And that's okay.
    lockable_store: Arc<LockableEventCacheStore>,

    /// The store itself.
    ///
    /// The store lives here, and inside `Self::lockable_store`.
    ///
    /// This is necessary because the lock methods return a guard that contains
    /// a reference to the store.
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
        let lockable_store = Arc::new(LockableEventCacheStore::new(store.clone()));

        Self {
            cross_process_lock: CrossProcessStoreLock::new(
                lockable_store.clone(),
                "default".to_owned(),
                holder,
            ),
            lockable_store,
            store,
        }
    }

    /// Acquire a spin lock (see [`CrossProcessStoreLock::spin_lock`]).
    ///
    /// It doesn't check whether the lock has been poisoned or not.
    /// A lock has been poisoned if it's been acquired from another holder.
    pub async fn lock_unchecked(&self) -> Result<EventCacheStoreLockGuard<'_>, LockStoreError> {
        match self.lock().await? {
            Ok(guard) => Ok(guard),
            Err(poison_error) => Ok(poison_error.into_inner()),
        }
    }

    /// Acquire a spin lock (see [`CrossProcessStoreLock::spin_lock`]).
    ///
    /// It **does** check whether the lock has been poisoned or not. Use
    /// [`EventCacheStoreLock::lock_unchecked`] if you don't want to check. A
    /// lock has been poisoned if it's been acquired from another holder.
    ///
    /// This method returns a first `Result` to handle the locking error. The
    /// `Ok` variant contains another `Result` to handle the locking poison.
    pub async fn lock(
        &self,
    ) -> Result<
        Result<
            EventCacheStoreLockGuard<'_>,
            EventCacheStoreLockPoisonError<EventCacheStoreLockGuard<'_>>,
        >,
        LockStoreError,
    > {
        let cross_process_lock_guard = self.cross_process_lock.spin_lock(None).await?;
        let event_cache_store_lock_guard =
            EventCacheStoreLockGuard { cross_process_lock_guard, store: self.store.deref() };

        Ok(if self.lockable_store.is_poisoned() {
            Err(EventCacheStoreLockPoisonError(event_cache_store_lock_guard))
        } else {
            Ok(event_cache_store_lock_guard)
        })
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
impl<'a> fmt::Debug for EventCacheStoreLockGuard<'a> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("EventCacheStoreLockGuard").finish_non_exhaustive()
    }
}

impl<'a> Deref for EventCacheStoreLockGuard<'a> {
    type Target = DynEventCacheStore;

    fn deref(&self) -> &Self::Target {
        self.store
    }
}

/// A type of error which can be returned whenever a cross-process lock is
/// acquired.
///
/// [`EventCacheStoreLock`] is poisoned whenever the lock is acquired from
/// another holder than the current holder, i.e. if the previous lock was held
/// by another process basically.
pub struct EventCacheStoreLockPoisonError<T>(T);

impl<T> EventCacheStoreLockPoisonError<T> {
    fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for EventCacheStoreLockPoisonError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventCacheStoreLockPoisonError").finish_non_exhaustive()
    }
}

impl<T> fmt::Display for EventCacheStoreLockPoisonError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        "Poisoned lock: lock has been acquired from another process".fmt(formatter)
    }
}

impl<T> Error for EventCacheStoreLockPoisonError<T> {
    fn description(&self) -> &str {
        "Poisoned lock: lock has been acquired from another process"
    }
}

/// Event cache store specific error type.
#[derive(Debug, thiserror::Error)]
pub enum EventCacheStoreError {
    /// An error happened in the underlying database backend.
    #[error(transparent)]
    Backend(Box<dyn Error + Send + Sync>),

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
        E: Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

/// An `EventCacheStore` specific result type.
pub type Result<T, E = EventCacheStoreError> = StdResult<T, E>;

/// A type that wraps the [`EventCacheStore`] but implements [`BackingStore`] to
/// make it usable inside the cross process lock.
#[derive(Clone, Debug)]
struct LockableEventCacheStore {
    store: Arc<DynEventCacheStore>,
    generation_and_is_poisoned: Arc<Mutex<(LockGeneration, bool)>>,
}

impl LockableEventCacheStore {
    fn new(store: Arc<DynEventCacheStore>) -> Self {
        Self {
            store,
            generation_and_is_poisoned: Arc::new(Mutex::new((FIRST_LOCK_GENERATION, false))),
        }
    }

    fn is_poisoned(&self) -> bool {
        self.generation_and_is_poisoned.lock().unwrap().1
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl BackingStore for LockableEventCacheStore {
    type LockError = EventCacheStoreError;

    async fn try_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> StdResult<bool, Self::LockError> {
        let lock_generation =
            self.store.try_take_leased_lock(lease_duration_ms, key, holder).await?;

        Ok(match lock_generation {
            // Lock hasn't been acquired.
            None => false,

            // Lock has been acquired, and we have a generation.
            Some(generation) => {
                let mut guard = self.generation_and_is_poisoned.lock().unwrap();
                let (last_generation, is_poisoned) = guard.deref_mut();

                // The lock is considered poisoned if it's been acquired
                // from another holder. If the lock is acquired from another
                // holder, its generation is incremented by one. So, if
                // `lock_generation` is different of `last_generation`, it
                // means it's been acquired from another holder, and it is
                // consequently poisoned; otherwise it is not poisoned.
                //
                // The initial value for `last_generation` **must be**
                // `FIRST_LOCK_GENERATION`.
                *is_poisoned = generation != *last_generation;
                *last_generation = generation;

                true
            }
        })
    }
}
