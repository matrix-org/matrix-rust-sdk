// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use crate::event_cache_store::{
    indexeddb_serializer::IndexeddbSerializer, migrations::open_and_upgrade_db,
};
use async_trait::async_trait;
use indexed_db_futures::IdbDatabase;
use matrix_sdk_base::event_cache::store::EventCacheStore;
use matrix_sdk_base::StoreError;
use matrix_sdk_store_encryption::{Error as EncryptionError, StoreCipher};
use std::sync::Arc;
use tracing::debug;

mod indexeddb_serializer;
mod migrations;

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbEventCacheStoreError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] EncryptionError),
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error(transparent)]
    StoreError(#[from] StoreError),
    #[error("Can't migrate {name} from {old_version} to {new_version} without deleting data. See MigrationConflictStrategy for ways to configure.")]
    MigrationConflict { name: String, old_version: u32, new_version: u32 },
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

impl From<IndexeddbEventCacheStoreError> for StoreError {
    fn from(e: IndexeddbEventCacheStoreError) -> Self {
        match e {
            IndexeddbEventCacheStoreError::Json(e) => StoreError::Json(e),
            IndexeddbEventCacheStoreError::StoreError(e) => e,
            IndexeddbEventCacheStoreError::Encryption(e) => StoreError::Encryption(e),
            _ => StoreError::backend(e),
        }
    }
}

type Result<A, E = IndexeddbEventCacheStoreError> = std::result::Result<A, E>;

/// Sometimes Migrations can't proceed without having to drop existing
/// data. This allows you to configure, how these cases should be handled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationConflictStrategy {
    /// Just drop the data, we don't care that we have to sync again
    Drop,
    /// Raise a [`IndexeddbStateStoreError::MigrationConflict`] error with the
    /// path to the DB in question. The caller then has to take care about
    /// what they want to do and try again after.
    Raise,
    /// Default.
    BackupAndDrop,
}

pub struct IndexeddbEventCacheStore {
    name: String,
    pub(crate) inner: IdbDatabase,
    pub(crate) serializer: IndexeddbSerializer,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for IndexeddbEventCacheStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbEventCacheStore").finish()
    }
}

impl IndexeddbEventCacheStore {
    pub fn builder() -> IndexeddbEventCacheStoreBuilder {
        IndexeddbEventCacheStoreBuilder::new()
    }
}

#[async_trait]
impl EventCacheStore for IndexeddbEventCacheStore {}

/// Builder for [`IndexeddbEventCacheStore`]
#[derive(Debug)]
pub struct IndexeddbEventCacheStoreBuilder {
    name: Option<String>,
    store_cipher: Option<Arc<StoreCipher>>,
    migration_conflict_strategy: MigrationConflictStrategy,
}

impl IndexeddbEventCacheStoreBuilder {
    fn new() -> Self {
        Self {
            name: None,
            store_cipher: None,
            migration_conflict_strategy: MigrationConflictStrategy::BackupAndDrop,
        }
    }

    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn store_cipher(mut self, store_cipher: Arc<StoreCipher>) -> Self {
        self.store_cipher = Some(store_cipher);
        self
    }

    /// The strategy to use when a merge conflict is found.
    ///
    /// See [`MigrationConflictStrategy`] for details.
    pub fn migration_conflict_strategy(mut self, value: MigrationConflictStrategy) -> Self {
        self.migration_conflict_strategy = value;
        self
    }

    pub async fn build(self) -> Result<IndexeddbEventCacheStore> {
        // let migration_strategy = self.migration_conflict_strategy.clone();
        let name = self.name.unwrap_or_else(|| "event_cache".to_owned());

        let serializer = IndexeddbSerializer::new(self.store_cipher);
        debug!("IndexedDbEventCacheStore: opening main store {name}");
        let inner = open_and_upgrade_db(&name, &serializer).await?;

        Ok(IndexeddbEventCacheStore { name, inner, serializer })
    }
}
