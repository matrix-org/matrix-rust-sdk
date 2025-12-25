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

// At the moment, this builder is not public outside of the crate, so we
// get a few dead code warnings; however, this will eventually be a public
// type, at which point the line below can be removed.
#![allow(dead_code)]

use std::{rc::Rc, sync::Arc};

use matrix_sdk_store_encryption::StoreCipher;

use crate::{
    event_cache_store::{
        IndexeddbEventCacheStore, error::IndexeddbEventCacheStoreError,
        migrations::open_and_upgrade_db,
    },
    serializer::{indexed_type::IndexedTypeSerializer, safe_encode::types::SafeEncodeSerializer},
};

/// A type for conveniently building an [`IndexeddbEventCacheStore`]
pub struct IndexeddbEventCacheStoreBuilder {
    // The name of the IndexedDB database which will be opened
    database_name: String,
    // The store cipher, if any, to use when encrypting data
    // before it is persisted to the IndexedDB database
    store_cipher: Option<Arc<StoreCipher>>,
}

impl Default for IndexeddbEventCacheStoreBuilder {
    fn default() -> Self {
        Self { database_name: Self::DEFAULT_DATABASE_NAME.to_owned(), store_cipher: None }
    }
}

impl IndexeddbEventCacheStoreBuilder {
    /// The default name of the IndexedDB database used to back the
    /// [`IndexeddbEventCacheStore`]
    pub const DEFAULT_DATABASE_NAME: &'static str = "event_cache";

    /// Sets the name of the IndexedDB database which will be opened. This
    /// defaults to [`Self::DEFAULT_DATABASE_NAME`].
    pub fn database_name(mut self, name: String) -> Self {
        self.database_name = name;
        self
    }

    /// Create a new [`IndexeddbEventCacheStoreBuilder`] where the database name
    /// is constructed by joining the given prefix with
    /// [`Self::DEFAULT_DATABASE_NAME`] and separated by `::`.
    pub fn with_prefix(prefix: &str) -> Self {
        Self {
            database_name: format!("{}::{}", prefix, Self::DEFAULT_DATABASE_NAME),
            store_cipher: None,
        }
    }

    /// Sets the store cipher to use when encrypting data before it is persisted
    /// to the IndexedDB database. By default, no store cipher is used -
    /// i.e., data is not encrypted before it is persisted.
    pub fn store_cipher(mut self, store_cipher: Arc<StoreCipher>) -> Self {
        self.store_cipher = Some(store_cipher);
        self
    }

    /// Opens the IndexedDB database with the provided name. If successfully
    /// opened, builds the [`IndexeddbEventCacheStore`] with that database
    /// and the provided store cipher.
    pub async fn build(self) -> Result<IndexeddbEventCacheStore, IndexeddbEventCacheStoreError> {
        Ok(IndexeddbEventCacheStore {
            inner: Rc::new(open_and_upgrade_db(&self.database_name).await?),
            serializer: IndexedTypeSerializer::new(SafeEncodeSerializer::new(self.store_cipher)),
        })
    }
}
