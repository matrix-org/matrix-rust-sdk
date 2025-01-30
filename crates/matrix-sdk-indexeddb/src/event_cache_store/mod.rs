use matrix::sdk::base::event_cache::store::EventCacheStore;

pub struct IndexeddbEventCacheStore {
    name: String,
    pub(crate) inner: IdbDatabase,
    pub(crate) meta: IdbDatabase,
    pub(crate) store_cipher: Option<Arc<StoreCipher>>,
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
    passphrase: Option<String>,
}

impl IndexeddbEventCacheStoreBuilder {
    fn new() -> Self {
        Self { name: None, passphrase: None }
    }

    pub fn name(mut self, name: String) -> Self {
        self.name = Some(name);
        self
    }

    pub fn passphrase(mut self, passphrase: String) -> Self {
        self.passphrase = Some(passphrase);
        self
    }

    pub async fn build(self) -> Result<IndexeddbEventCacheStore, StoreError> {
        let name = self.name.unwrap_or_else(|| "event_cache".to_owned());

        let (meta, store_cipher) = upgrade_meta_db(&meta_name, self.passphrase.as_deref()).await?;
        let inner = upgrade_inner_db(&name, store_cipher.as_deref(), &meta).await?;

        Ok(IndexeddbEventCacheStore { name, inner, meta, store_cipher })
    }
}
