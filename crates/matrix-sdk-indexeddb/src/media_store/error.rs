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
    media::store::{MediaStore, MediaStoreError, MemoryMediaStore},
    SendOutsideWasm, SyncOutsideWasm,
};
use thiserror::Error;

use crate::media_store::transaction::IndexeddbMediaStoreTransactionError;

/// A trait that combines the necessary traits needed for asynchronous runtimes,
/// but excludes them when running in a web environment - i.e., when
/// `#[cfg(target_family = "wasm")]`.
pub trait AsyncErrorDeps: std::error::Error + SendOutsideWasm + SyncOutsideWasm + 'static {}

impl<T> AsyncErrorDeps for T where T: std::error::Error + SendOutsideWasm + SyncOutsideWasm + 'static
{}

#[derive(Debug, Error)]
pub enum IndexeddbMediaStoreError {
    #[error("media store: {0}")]
    MemoryStore(<MemoryMediaStore as MediaStore>::Error),

    #[error("transaction: {0}")]
    Transaction(#[from] IndexeddbMediaStoreTransactionError),

    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
}

impl From<IndexeddbMediaStoreError> for MediaStoreError {
    fn from(value: IndexeddbMediaStoreError) -> Self {
        use IndexeddbMediaStoreError::*;

        match value {
            DomException { .. } => Self::InvalidData { details: value.to_string() },
            Transaction(inner) => inner.into(),
            MemoryStore(error) => error,
        }
    }
}

impl From<web_sys::DomException> for IndexeddbMediaStoreError {
    fn from(value: web_sys::DomException) -> Self {
        Self::DomException { name: value.name(), message: value.message(), code: value.code() }
    }
}
