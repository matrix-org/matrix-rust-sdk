use super::{migrations::MigrationConflictStrategy, IndexeddbEventCacheStore, Result};
use crate::event_cache_store::{
    indexeddb_serializer::IndexeddbSerializer, migrations::open_and_upgrade_db,
};

use matrix_sdk_base::event_cache::store::media::{MediaRetentionPolicy, MediaService};
use matrix_sdk_store_encryption::StoreCipher;
use std::sync::Arc;

/// Builder for [`IndexeddbEventCacheStore`]
// #[derive(Debug)] // TODO StoreCipher cannot be derived
pub struct IndexeddbEventCacheStoreBuilder {
    name: Option<String>,
    store_cipher: Option<Arc<StoreCipher>>,
    migration_conflict_strategy: MigrationConflictStrategy,
}

impl IndexeddbEventCacheStoreBuilder {
    pub fn new() -> Self {
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
        let inner = open_and_upgrade_db(&name, &serializer).await?;

        let media_service = MediaService::new();
        let media_retention_policy: Option<MediaRetentionPolicy> =
            media_retention_policy(&inner).await?;
        media_service.restore(policy);

        let store =
            IndexeddbEventCacheStore { inner, serializer, media_service: Arc::new(media_service) };
        Ok(store)
    }
}

impl Default for IndexeddbEventCacheStoreBuilder {
    fn default() -> Self {
        Self::new()
    }
}
