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

use matrix_sdk_base::{
    event_cache::store::{EventCacheStore, EventCacheStoreError, MemoryStore},
    SendOutsideWasm, SyncOutsideWasm,
};
use serde::de::Error;
use thiserror::Error;

use crate::event_cache_store::transaction::IndexeddbEventCacheStoreTransactionError;

/// A trait that combines the necessary traits needed for asynchronous runtimes,
/// but excludes them when running in a web environment - i.e., when
/// `#[cfg(target_family = "wasm")]`.
pub trait AsyncErrorDeps: std::error::Error + SendOutsideWasm + SyncOutsideWasm + 'static {}

impl<T> AsyncErrorDeps for T where T: std::error::Error + SendOutsideWasm + SyncOutsideWasm + 'static
{}

#[derive(Debug, Error)]
pub enum IndexeddbEventCacheStoreError {
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error("transaction: {0}")]
    Transaction(#[from] IndexeddbEventCacheStoreTransactionError),
    #[error("media store: {0}")]
    MemoryStore(<MemoryStore as EventCacheStore>::Error),
}

impl From<web_sys::DomException> for IndexeddbEventCacheStoreError {
    fn from(value: web_sys::DomException) -> IndexeddbEventCacheStoreError {
        IndexeddbEventCacheStoreError::DomException {
            name: value.name(),
            message: value.message(),
            code: value.code(),
        }
    }
}

impl From<IndexeddbEventCacheStoreError> for EventCacheStoreError {
    fn from(value: IndexeddbEventCacheStoreError) -> Self {
        match value {
            IndexeddbEventCacheStoreError::DomException { .. } => {
                Self::InvalidData { details: value.to_string() }
            }
            IndexeddbEventCacheStoreError::Transaction(ref inner) => match inner {
                IndexeddbEventCacheStoreTransactionError::DomException { .. } => {
                    Self::InvalidData { details: value.to_string() }
                }
                IndexeddbEventCacheStoreTransactionError::Serialization(e) => {
                    Self::Serialization(serde_json::Error::custom(e.to_string()))
                }
                IndexeddbEventCacheStoreTransactionError::ItemIsNotUnique => {
                    Self::InvalidData { details: value.to_string() }
                }
            },
            IndexeddbEventCacheStoreError::MemoryStore(inner) => inner,
        }
    }
}
