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

mod builder;
mod migrations;

use std::sync::Arc;

pub use builder::IndexeddbEventCacheStoreBuilder;
use indexed_db_futures::IdbDatabase;
use matrix_sdk_base::event_cache::store::EventCacheStoreError;
use matrix_sdk_store_encryption::StoreCipher;

use crate::serializer::IndexeddbSerializer;

mod keys {
    pub const CORE: &str = "core";
    pub const EVENTS: &str = "events";
    pub const LINKED_CHUNKS: &str = "linked_chunks";
    pub const GAPS: &str = "gaps";
}

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbEventCacheStoreError {
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
}

impl From<web_sys::DomException> for IndexeddbEventCacheStoreError {
    fn from(frm: web_sys::DomException) -> IndexeddbEventCacheStoreError {
        IndexeddbEventCacheStoreError::DomException {
            name: frm.name(),
            message: frm.message(),
            code: frm.code(),
        }
    }
}

impl From<IndexeddbEventCacheStoreError> for EventCacheStoreError {
    fn from(e: IndexeddbEventCacheStoreError) -> Self {
        match e {
            IndexeddbEventCacheStoreError::DomException { .. } => EventCacheStoreError::backend(e),
        }
    }
}

type Result<T, E = IndexeddbEventCacheStoreError> = std::result::Result<T, E>;

pub struct IndexeddbEventCacheStore {
    pub inner: IdbDatabase,
    pub store_cipher: Option<Arc<StoreCipher>>,
    pub serializer: IndexeddbSerializer,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for IndexeddbEventCacheStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbEventCacheStore")
            .field("inner", &self.inner)
            .field("store_cipher", &self.store_cipher.as_ref().map(|_| "<StoreCipher>"))
            .field("serializer", &self.serializer)
            .finish()
    }
}

impl IndexeddbEventCacheStore {
    pub fn builder() -> IndexeddbEventCacheStoreBuilder {
        IndexeddbEventCacheStoreBuilder::new()
    }
}
