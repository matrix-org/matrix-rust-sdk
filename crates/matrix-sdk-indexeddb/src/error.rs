// Copyright 2025 The Matrix.org Foundation C.I.C.
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
// limitations under the License

#[cfg(feature = "state-store")]
use matrix_sdk_base::StoreError;
#[cfg(feature = "event-cache-store")]
use matrix_sdk_base::event_cache::store::EventCacheStoreError;
#[cfg(feature = "media-store")]
use matrix_sdk_base::media::store::MediaStoreError;
#[cfg(any(feature = "event-cache-store", feature = "media-store"))]
use matrix_sdk_base::{SendOutsideWasm, SyncOutsideWasm};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_crypto::CryptoStoreError;
use thiserror::Error;

/// A trait that combines the necessary traits needed for asynchronous runtimes,
/// but excludes them when running in a web environment - i.e., when
/// `#[cfg(target_family = "wasm")]`.
#[cfg(any(feature = "event-cache-store", feature = "media-store"))]
pub trait AsyncErrorDeps: std::error::Error + SendOutsideWasm + SyncOutsideWasm + 'static {}

#[cfg(any(feature = "event-cache-store", feature = "media-store"))]
impl<T> AsyncErrorDeps for T where T: std::error::Error + SendOutsideWasm + SyncOutsideWasm + 'static
{}

/// A wrapper around [`String`] that derives [`Error`](std::error::Error).
/// This is useful when a particular error is not [`Send`] or [`Sync`] but
/// must be mapped into a higher-level error that requires those constraints,
/// e.g. [`StoreError::Backend`], [`CryptStoreError::Backend`], etc.
#[derive(Debug, Error)]
#[error("{0}")]
pub struct GenericError(String);

impl<S: AsRef<str>> From<S> for GenericError {
    fn from(value: S) -> Self {
        Self(value.as_ref().to_owned())
    }
}

#[cfg(feature = "e2e-encryption")]
impl From<GenericError> for CryptoStoreError {
    fn from(value: GenericError) -> Self {
        Self::backend(value)
    }
}

#[cfg(feature = "event-cache-store")]
impl From<GenericError> for EventCacheStoreError {
    fn from(value: GenericError) -> Self {
        Self::backend(value)
    }
}

#[cfg(feature = "media-store")]
impl From<GenericError> for MediaStoreError {
    fn from(value: GenericError) -> Self {
        Self::backend(value)
    }
}

#[cfg(feature = "state-store")]
impl From<GenericError> for StoreError {
    fn from(value: GenericError) -> Self {
        Self::backend(value)
    }
}
