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
#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;

use matrix_sdk_store_encryption::Error as StoreEncryptionError;

#[cfg(any(test, feature = "testing"))]
pub use self::integration_tests::{MediaStoreInnerIntegrationTests, MediaStoreIntegrationTests};
pub use self::{
    media_retention_policy::MediaRetentionPolicy,
    media_service::{IgnoreMediaRetentionPolicy, MediaService, MediaStore, MediaStoreInner},
    memory_store::MemoryMediaStore,
};

/// Media store specific error type.
#[derive(Debug, thiserror::Error)]
pub enum MediaStoreError {
    /// An error happened in the underlying database backend.
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),

    /// The store failed to encrypt or decrypt some data.
    #[error("Error encrypting or decrypting data from the event cache store: {0}")]
    Encryption(#[from] StoreEncryptionError),

    /// The store contains invalid data.
    #[error("The store contains invalid data: {details}")]
    InvalidData {
        /// Details why the data contained in the store was invalid.
        details: String,
    },

    /// The store failed to serialize or deserialize some data.
    #[error("Error serializing or deserializing data from the event cache store: {0}")]
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

/// An `MediaStore` specific result type.
pub type Result<T, E = MediaStoreError> = std::result::Result<T, E>;
