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

use std::{str::Utf8Error, time::Duration};

#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;
mod memory_store;
mod traits;
mod wrapper;

pub use matrix_sdk_store_encryption::Error as StoreEncryptionError;
use ruma::time::SystemTime;
use serde::{Deserialize, Serialize};

#[cfg(any(test, feature = "testing"))]
pub use self::integration_tests::EventCacheStoreIntegrationTests;
pub use self::{
    memory_store::MemoryStore,
    traits::{DynEventCacheStore, EventCacheStore, IntoEventCacheStore},
    wrapper::EventCacheStoreWrapper,
};

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

/// The retention policy for media content used by the `EventCacheStore`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRetentionPolicy {
    /// The maximum authorized size of the overall media cache, in bytes.
    ///
    /// The cache size is defined as the sum of the sizes of all the (possibly
    /// encrypted) media contents in the cache, excluding any metadata
    /// associated with them.
    ///
    /// If this is set and the cache size is bigger than this value, the oldest
    /// media contents in the cache will be removed during a cleanup until the
    /// cache size is below this threshold.
    ///
    /// Note that it is possible for the cache size to temporarily exceed this
    /// value between two cleanups.
    ///
    /// Defaults to 400 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cache_size: Option<usize>,
    /// The maximum authorized size of a single media content, in bytes.
    ///
    /// The size of a media content is the size taken by the content in the
    /// database, after it was possibly encrypted, so it might differ from the
    /// initial size of the content.
    ///
    /// The maximum authorized size of a single media content is actually the
    /// lowest value between `max_cache_size` and `max_file_size`.
    ///
    /// If it is set, media content bigger than the maximum size will not be
    /// cached. If the maximum size changed after media content that exceeds the
    /// new value was cached, the corresponding content will be removed
    /// during a cleanup.
    ///
    /// Defaults to 20 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_size: Option<usize>,
    /// The duration after which unaccessed media content is considered
    /// expired.
    ///
    /// If this is set, media content whose last access is older than this
    /// duration will be removed from the media cache during a cleanup.
    ///
    /// Defaults to 30 days.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_access_expiry: Option<Duration>,
}

impl MediaRetentionPolicy {
    /// Create a `MediaRetentionPolicy` with the default values.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an empty `MediaRetentionPolicy`.
    ///
    /// This means that all media will be cached and cleanups have no effect.
    pub fn empty() -> Self {
        Self { max_cache_size: None, max_file_size: None, last_access_expiry: None }
    }

    /// Set the maximum authorized size of the overall media cache, in bytes.
    pub fn with_max_cache_size(mut self, size: Option<usize>) -> Self {
        self.max_cache_size = size;
        self
    }

    /// Set the maximum authorized size of a single media content, in bytes.
    pub fn with_max_file_size(mut self, size: Option<usize>) -> Self {
        self.max_file_size = size;
        self
    }

    /// Set the duration before which unaccessed media content is considered
    /// expired.
    pub fn with_last_access_expiry(mut self, duration: Option<Duration>) -> Self {
        self.last_access_expiry = duration;
        self
    }

    /// Whether this policy has limitations.
    ///
    /// If this policy has no limitations, a cleanup job would have no effect.
    ///
    /// Returns `true` if at least one limitation is set.
    pub fn has_limitations(&self) -> bool {
        self.max_cache_size.is_some()
            || self.max_file_size.is_some()
            || self.last_access_expiry.is_some()
    }

    /// Whether the given size exceeds the maximum authorized size of the media
    /// cache.
    ///
    /// # Arguments
    ///
    /// * `size` - The overall size of the media cache to check, in bytes.
    pub fn exceeds_max_cache_size(&self, size: usize) -> bool {
        self.max_cache_size.is_some_and(|max_size| size > max_size)
    }

    /// The computed maximum authorized size of a single media content, in
    /// bytes.
    ///
    /// This is the lowest value between `max_cache_size` and `max_file_size`.
    pub fn computed_max_file_size(&self) -> Option<usize> {
        match (self.max_cache_size, self.max_file_size) {
            (None, None) => None,
            (None, Some(size)) => Some(size),
            (Some(size), None) => Some(size),
            (Some(max_cache_size), Some(max_file_size)) => Some(max_cache_size.min(max_file_size)),
        }
    }

    /// Whether the given size, in bytes, exceeds the computed maximum
    /// authorized size of a single media content.
    ///
    /// # Arguments
    ///
    /// * `size` - The size of the media content to check, in bytes.
    pub fn exceeds_max_file_size(&self, size: usize) -> bool {
        self.computed_max_file_size().is_some_and(|max_size| size > max_size)
    }

    /// Whether a content whose last access was at the given time has expired.
    ///
    /// # Arguments
    ///
    /// * `current_time` - The current time.
    ///
    /// * `last_access_time` - The time when the media content to check was last
    ///   accessed.
    pub fn has_content_expired(
        &self,
        current_time: SystemTime,
        last_access_time: SystemTime,
    ) -> bool {
        self.last_access_expiry.is_some_and(|max_duration| {
            current_time
                .duration_since(last_access_time)
                // If this returns an error, the last access time is newer than the current time.
                // This shouldn't happen but in this case the content cannot be expired.
                .is_ok_and(|elapsed| elapsed >= max_duration)
        })
    }
}

impl Default for MediaRetentionPolicy {
    fn default() -> Self {
        Self {
            // 400 MiB.
            max_cache_size: Some(400 * 1024 * 1024),
            // 20 MiB.
            max_file_size: Some(20 * 1024 * 1024),
            // 30 days.
            last_access_expiry: Some(Duration::from_secs(30 * 24 * 60 * 60)),
        }
    }
}
