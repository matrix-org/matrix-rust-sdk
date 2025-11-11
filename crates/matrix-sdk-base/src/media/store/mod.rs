// Copyright 2025 Kévin Commaille
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

//! The media store holds downloaded media when the cache was
//! activated to save bandwidth at the cost of increased storage space usage.
//!
//! Implementing the `MediaStore` trait, you can plug any storage backend
//! into the media store for the actual storage. By default this brings an
//! in-memory store.

mod media_retention_policy;
mod media_service;
mod memory_store;
mod traits;
#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;

#[cfg(not(tarpaulin_include))]
use std::fmt;
use std::{ops::Deref, sync::Arc};

use matrix_sdk_common::cross_process_lock::{
    CrossProcessLock, CrossProcessLockError, CrossProcessLockGeneration, CrossProcessLockGuard,
    CrossProcessLockState, TryLock,
};
use matrix_sdk_store_encryption::Error as StoreEncryptionError;
pub use traits::{DynMediaStore, IntoMediaStore, MediaStore, MediaStoreInner};

#[cfg(any(test, feature = "testing"))]
pub use self::integration_tests::{MediaStoreInnerIntegrationTests, MediaStoreIntegrationTests};
pub use self::{
    media_retention_policy::MediaRetentionPolicy,
    media_service::{IgnoreMediaRetentionPolicy, MediaService},
    memory_store::MemoryMediaStore,
};

/// Media store specific error type.
#[derive(Debug, thiserror::Error)]
pub enum MediaStoreError {
    /// An error happened in the underlying database backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),

    /// The store failed to encrypt or decrypt some data.
    #[error("Error encrypting or decrypting data from the media store: {0}")]
    Encryption(#[from] StoreEncryptionError),

    /// The store contains invalid data.
    #[error("The store contains invalid data: {details}")]
    InvalidData {
        /// Details why the data contained in the store was invalid.
        details: String,
    },

    /// The store failed to serialize or deserialize some data.
    #[error("Error serializing or deserializing data from the media store: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl MediaStoreError {
    /// Create a new [`Backend`][Self::Backend] error.
    ///
    /// Shorthand for `MediaStoreError::Backend(Box::new(error))`.
    #[inline]
    pub fn backend<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

impl From<MediaStoreError> for CrossProcessLockError {
    fn from(value: MediaStoreError) -> Self {
        Self::TryLock(Box::new(value))
    }
}

/// An `MediaStore` specific result type.
pub type Result<T, E = MediaStoreError> = std::result::Result<T, E>;

/// The high-level public type to represent an `MediaStore` lock.
#[derive(Clone)]
pub struct MediaStoreLock {
    /// The inner cross process lock that is used to lock the `MediaStore`.
    cross_process_lock: Arc<CrossProcessLock<LockableMediaStore>>,

    /// The store itself.
    ///
    /// That's the only place where the store exists.
    store: Arc<DynMediaStore>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for MediaStoreLock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("MediaStoreLock").finish_non_exhaustive()
    }
}

impl MediaStoreLock {
    /// Create a new lock around the [`MediaStore`].
    ///
    /// The `holder` argument represents the holder inside the
    /// [`CrossProcessLock::new`].
    pub fn new<S>(store: S, holder: String) -> Self
    where
        S: IntoMediaStore,
    {
        let store = store.into_media_store();

        Self {
            cross_process_lock: Arc::new(CrossProcessLock::new(
                LockableMediaStore(store.clone()),
                "default".to_owned(),
                holder,
            )),
            store,
        }
    }

    /// Acquire a spin lock (see [`CrossProcessLock::spin_lock`]).
    pub async fn lock(&self) -> Result<MediaStoreLockGuard<'_>, CrossProcessLockError> {
        let cross_process_lock_guard = match self.cross_process_lock.spin_lock(None).await?? {
            // The lock is clean: no other hold acquired it, all good!
            CrossProcessLockState::Clean(guard) => guard,

            // The lock is dirty: another holder acquired it since the last time we acquired it.
            // It's not a problem in the case of the `MediaStore` because this API is “stateless” at
            // the time of writing (2025-11-11). There is nothing that can be out-of-sync: all the
            // state is in the database, nothing in memory.
            CrossProcessLockState::Dirty(guard) => {
                guard.clear_dirty();

                guard
            }
        };

        Ok(MediaStoreLockGuard { cross_process_lock_guard, store: self.store.deref() })
    }
}

/// An RAII implementation of a “scoped lock” of an [`MediaStoreLock`].
/// When this structure is dropped (falls out of scope), the lock will be
/// unlocked.
pub struct MediaStoreLockGuard<'a> {
    /// The cross process lock guard.
    #[allow(unused)]
    cross_process_lock_guard: CrossProcessLockGuard,

    /// A reference to the store.
    store: &'a DynMediaStore,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for MediaStoreLockGuard<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("MediaStoreLockGuard").finish_non_exhaustive()
    }
}

impl Deref for MediaStoreLockGuard<'_> {
    type Target = DynMediaStore;

    fn deref(&self) -> &Self::Target {
        self.store
    }
}

/// A type that wraps the [`MediaStore`] but implements [`TryLock`] to
/// make it usable inside the cross process lock.
#[derive(Clone, Debug)]
struct LockableMediaStore(Arc<DynMediaStore>);

impl TryLock for LockableMediaStore {
    type LockError = MediaStoreError;

    async fn try_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> std::result::Result<Option<CrossProcessLockGeneration>, Self::LockError> {
        self.0.try_take_leased_lock(lease_duration_ms, key, holder).await
    }
}
