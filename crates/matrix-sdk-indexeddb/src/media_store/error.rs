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

use matrix_sdk_base::media::store::{MediaStore, MediaStoreError, MemoryMediaStore};
use serde::de::Error;
use thiserror::Error;

use crate::{error::GenericError, transaction::TransactionError};

#[derive(Debug, Error)]
pub enum IndexeddbMediaStoreError {
    #[error("unable to open database: {0}")]
    UnableToOpenDatabase(String),
    #[error("media store: {0}")]
    MemoryStore(<MemoryMediaStore as MediaStore>::Error),
    #[error("transaction: {0}")]
    Transaction(#[from] TransactionError),
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error("cache size too big, cannot exceed 'usize::MAX' ({})", usize::MAX)]
    CacheSizeTooBig,
}

impl From<IndexeddbMediaStoreError> for MediaStoreError {
    fn from(value: IndexeddbMediaStoreError) -> Self {
        use IndexeddbMediaStoreError::*;

        match value {
            UnableToOpenDatabase(e) => GenericError::from(e).into(),
            DomException { .. } => Self::InvalidData { details: value.to_string() },
            Transaction(inner) => inner.into(),
            CacheSizeTooBig => GenericError::from(value.to_string()).into(),
            MemoryStore(error) => error,
        }
    }
}

impl From<web_sys::DomException> for IndexeddbMediaStoreError {
    fn from(value: web_sys::DomException) -> Self {
        Self::DomException { name: value.name(), message: value.message(), code: value.code() }
    }
}

impl From<indexed_db_futures::error::OpenDbError> for IndexeddbMediaStoreError {
    fn from(value: indexed_db_futures::error::OpenDbError) -> Self {
        use indexed_db_futures::error::OpenDbError::*;
        match value {
            VersionZero | UnsupportedEnvironment | NullFactory => {
                Self::UnableToOpenDatabase(value.to_string())
            }
            Base(e) => TransactionError::from(e).into(),
        }
    }
}

impl From<TransactionError> for MediaStoreError {
    fn from(value: TransactionError) -> Self {
        use TransactionError::*;

        match value {
            DomException { .. } => Self::InvalidData { details: value.to_string() },
            Serialization(e) => Self::Serialization(serde_json::Error::custom(e.to_string())),
            ItemIsNotUnique | ItemNotFound | NumericalOverflow => {
                Self::InvalidData { details: value.to_string() }
            }
            Backend(e) => GenericError::from(e.to_string()).into(),
        }
    }
}
