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
#[cfg(any(test, feature = "testing"))]
#[macro_use]
pub mod integration_tests;

#[cfg(any(test, feature = "testing"))]
pub use self::integration_tests::MediaStoreInnerIntegrationTests;
pub use self::{
    media_retention_policy::MediaRetentionPolicy,
    media_service::{IgnoreMediaRetentionPolicy, MediaService, MediaStoreInner},
};

/// Media store specific error type.
#[derive(Debug, thiserror::Error)]
pub enum MediaStoreError {}

/// An `MediaStore` specific result type.
pub type Result<T, E = MediaStoreError> = std::result::Result<T, E>;
