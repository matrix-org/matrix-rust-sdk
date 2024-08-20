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

//! The media cache holds all downloaded media when the cache was activated to
//! save bandwidth at the cost of increased storage space usage.
//!
//! Implementing the `MediaCache` trait, you can plug any storage backend
//! into the cache for the actual storage. By default this brings an in-memory
//! store.

use std::str::Utf8Error;

#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;
mod memory_store;
mod traits;

pub use matrix_sdk_store_encryption::Error as StoreEncryptionError;

#[cfg(any(test, feature = "testing"))]
pub use self::integration_tests::MediaCacheIntegrationTests;
pub use self::{
    memory_store::MemoryStore,
    traits::{DynMediaCache, IntoMediaCache, MediaCache},
};

/// Media cache specific error type.
#[derive(Debug, thiserror::Error)]
pub enum MediaCacheError {
    /// An error happened in the underlying database backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),

    /// The media cache is locked with a passphrase and an incorrect passphrase
    /// was given.
    #[error("The media cache failed to be unlocked")]
    Locked,

    /// An unencrypted media cache was tried to be unlocked with a passphrase.
    #[error("The media cache is not encrypted but was tried to be opened with a passphrase")]
    Unencrypted,

    /// The media cache failed to encrypt or decrypt some data.
    #[error("Error encrypting or decrypting data from the media cache: {0}")]
    Encryption(#[from] StoreEncryptionError),

    /// The media cache failed to encode or decode some data.
    #[error("Error encoding or decoding data from the media cache: {0}")]
    Codec(#[from] Utf8Error),

    /// The database format has changed in a backwards incompatible way.
    #[error(
        "The database format changed in an incompatible way, current \
        version: {0}, latest version: {1}"
    )]
    UnsupportedDatabaseVersion(usize, usize),
}

impl MediaCacheError {
    /// Create a new [`Backend`][Self::Backend] error.
    ///
    /// Shorthand for `MediaCacheError::Backend(Box::new(error))`.
    #[inline]
    pub fn backend<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

/// A `MediaCache` specific result type.
pub type Result<T, E = MediaCacheError> = std::result::Result<T, E>;
