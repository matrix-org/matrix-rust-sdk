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

//! The event cache stores holds events when the cache was
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

use matrix_sdk_common::cross_process_lock::{
    CrossProcessLock, CrossProcessLockError, CrossProcessLockGeneration, CrossProcessLockGuard,
    MappedCrossProcessLockState, TryLock,
};
pub use matrix_sdk_store_encryption::Error as StoreEncryptionError;
use ruma::{OwnedEventId, events::AnySyncTimelineEvent, serde::Raw};
use tracing::trace;

#[cfg(any(test, feature = "testing"))]
pub use self::integration_tests::EventCacheStoreIntegrationTests;
pub use self::{
    memory_store::MemoryStore,
    traits::{DEFAULT_CHUNK_CAPACITY, DynEventCacheStore, EventCacheStore, IntoEventCacheStore},
};

/// The high-level public type to represent an `EventCacheStore` lock.
#[derive(Clone)]
pub struct EventCacheStoreLock {
    /// The inner cross process lock that is used to lock the `EventCacheStore`.
    cross_process_lock: Arc<CrossProcessLock<LockableEventCacheStore>>,

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
    /// [`CrossProcessLock::new`].
    pub fn new<S>(store: S, holder: String) -> Self
    where
        S: IntoEventCacheStore,
    {
        let store = store.into_event_cache_store();

        Self {
            cross_process_lock: Arc::new(CrossProcessLock::new(
                LockableEventCacheStore(store.clone()),
                "default".to_owned(),
                holder,
            )),
            store,
        }
    }

    /// Acquire a spin lock (see [`CrossProcessLock::spin_lock`]).
    pub async fn lock(&self) -> Result<EventCacheStoreLockState, CrossProcessLockError> {
        let lock_state =
            self.cross_process_lock.spin_lock(None).await??.map(|cross_process_lock_guard| {
                EventCacheStoreLockGuard { cross_process_lock_guard, store: self.store.clone() }
            });

        Ok(lock_state)
    }
}

/// The equivalent of [`CrossProcessLockState`] but for the [`EventCacheStore`].
///
/// [`CrossProcessLockState`]: matrix_sdk_common::cross_process_lock::CrossProcessLockState
pub type EventCacheStoreLockState = MappedCrossProcessLockState<EventCacheStoreLockGuard>;

/// An RAII implementation of a “scoped lock” of an [`EventCacheStoreLock`].
/// When this structure is dropped (falls out of scope), the lock will be
/// unlocked.
#[derive(Clone)]
pub struct EventCacheStoreLockGuard {
    /// The cross process lock guard.
    #[allow(unused)]
    cross_process_lock_guard: CrossProcessLockGuard,

    /// A reference to the store.
    store: Arc<DynEventCacheStore>,
}

impl EventCacheStoreLockGuard {
    /// Forward to [`CrossProcessLockGuard::clear_dirty`].
    ///
    /// This is an associated method to avoid colliding with the [`Deref`]
    /// implementation.
    pub fn clear_dirty(this: &Self) {
        this.cross_process_lock_guard.clear_dirty();
    }

    /// Force to [`CrossProcessLockGuard::is_dirty`].
    pub fn is_dirty(this: &Self) -> bool {
        this.cross_process_lock_guard.is_dirty()
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for EventCacheStoreLockGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("EventCacheStoreLockGuard").finish_non_exhaustive()
    }
}

impl Deref for EventCacheStoreLockGuard {
    type Target = DynEventCacheStore;

    fn deref(&self) -> &Self::Target {
        self.store.as_ref()
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

    /// The store contains invalid data.
    #[error("The store contains invalid data: {details}")]
    InvalidData {
        /// Details why the data contained in the store was invalid.
        details: String,
    },
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

impl From<EventCacheStoreError> for CrossProcessLockError {
    fn from(value: EventCacheStoreError) -> Self {
        Self::TryLock(Box::new(value))
    }
}

/// An `EventCacheStore` specific result type.
pub type Result<T, E = EventCacheStoreError> = std::result::Result<T, E>;

/// A type that wraps the [`EventCacheStore`] but implements [`TryLock`] to
/// make it usable inside the cross process lock.
#[derive(Clone, Debug)]
struct LockableEventCacheStore(Arc<DynEventCacheStore>);

impl TryLock for LockableEventCacheStore {
    type LockError = EventCacheStoreError;

    async fn try_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> std::result::Result<Option<CrossProcessLockGeneration>, Self::LockError> {
        self.0.try_take_leased_lock(lease_duration_ms, key, holder).await
    }
}

/// Helper to extract the relation information from an event.
///
/// If the event isn't in relation to another event, then this will return
/// `None`. Otherwise, returns both the event id this event relates to, and the
/// kind of relation as a string (e.g. `m.replace`).
pub fn extract_event_relation(event: &Raw<AnySyncTimelineEvent>) -> Option<(OwnedEventId, String)> {
    #[derive(serde::Deserialize)]
    struct RelatesTo {
        event_id: OwnedEventId,
        rel_type: String,
    }

    #[derive(serde::Deserialize)]
    struct EventContent {
        #[serde(rename = "m.relates_to")]
        rel: Option<RelatesTo>,
    }

    match event.get_field::<EventContent>("content") {
        Ok(event_content) => {
            event_content.and_then(|c| c.rel).map(|rel| (rel.event_id, rel.rel_type))
        }
        Err(err) => {
            trace!("when extracting relation data from an event: {err}");
            None
        }
    }
}
