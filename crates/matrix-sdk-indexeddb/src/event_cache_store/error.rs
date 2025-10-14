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

use matrix_sdk_base::event_cache::store::EventCacheStoreError;
use serde::de::Error;
use thiserror::Error;

use crate::{error::GenericError, transaction::TransactionError};

#[derive(Debug, Error)]
pub enum IndexeddbEventCacheStoreError {
    #[error("unable to open database: {0}")]
    UnableToOpenDatabase(String),
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error("chunks contain disjoint lists")]
    ChunksContainDisjointLists,
    #[error("chunks contain cycle")]
    ChunksContainCycle,
    #[error("unable to load chunk")]
    UnableToLoadChunk,
    #[error("no max chunk id")]
    NoMaxChunkId,
    #[error("transaction: {0}")]
    Transaction(#[from] TransactionError),
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

impl From<indexed_db_futures::error::OpenDbError> for IndexeddbEventCacheStoreError {
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

impl From<IndexeddbEventCacheStoreError> for EventCacheStoreError {
    fn from(value: IndexeddbEventCacheStoreError) -> Self {
        use IndexeddbEventCacheStoreError::*;

        match value {
            UnableToOpenDatabase(e) => GenericError::from(e).into(),
            DomException { .. }
            | ChunksContainCycle
            | ChunksContainDisjointLists
            | NoMaxChunkId
            | UnableToLoadChunk => Self::InvalidData { details: value.to_string() },
            Transaction(inner) => inner.into(),
        }
    }
}

impl From<TransactionError> for EventCacheStoreError {
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
