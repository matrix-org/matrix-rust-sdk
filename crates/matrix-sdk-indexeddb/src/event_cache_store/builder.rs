use std::sync::Arc;

use matrix_sdk_base::event_cache::store::MemoryStore;
use matrix_sdk_store_encryption::StoreCipher;

use crate::{
    event_cache_store::{migrations::open_and_upgrade_db, IndexeddbEventCacheStore, Result},
    serializer::IndexeddbSerializer,
};

/// Builder for [`IndexeddbEventCacheStore`]
pub struct IndexeddbEventCacheStoreBuilder {
    name: Option<String>,
    store_cipher: Option<Arc<StoreCipher>>,
}

impl IndexeddbEventCacheStoreBuilder {
    pub fn new() -> Self {
        Self { name: None, store_cipher: None }
    }

    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn store_cipher(mut self, store_cipher: Arc<StoreCipher>) -> Self {
        self.store_cipher = Some(store_cipher);
        self
    }

    pub async fn build(self) -> Result<IndexeddbEventCacheStore> {
        let name = self.name.unwrap_or_else(|| "event_cache".to_owned());
        let store = IndexeddbEventCacheStore {
            inner: open_and_upgrade_db(&name).await?,
            serializer: IndexeddbSerializer::new(self.store_cipher),
            media_store: MemoryStore::new(),
        };
        Ok(store)
    }
}

impl Default for IndexeddbEventCacheStoreBuilder {
    fn default() -> Self {
        Self::new()
    }
}
