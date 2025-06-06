#![cfg_attr(not(target_family = "wasm"), allow(unused))]

#[cfg(feature = "state-store")]
use matrix_sdk_base::store::StoreError;
use thiserror::Error;

#[cfg(feature = "e2e-encryption")]
mod crypto_store;
#[cfg(feature = "event-cache-store")]
mod event_cache_store;
mod safe_encode;
#[cfg(feature = "e2e-encryption")]
mod serialize_bool_for_indexeddb;
#[cfg(feature = "e2e-encryption")]
mod serializer;
#[cfg(feature = "state-store")]
mod state_store;

#[cfg(feature = "e2e-encryption")]
pub use crypto_store::{IndexeddbCryptoStore, IndexeddbCryptoStoreError};
#[cfg(feature = "event-cache-store")]
pub use event_cache_store::{
    IndexeddbEventCacheStore, IndexeddbEventCacheStoreBuilder, IndexeddbEventCacheStoreError,
};
#[cfg(feature = "state-store")]
pub use state_store::{
    IndexeddbStateStore, IndexeddbStateStoreBuilder, IndexeddbStateStoreError,
    MigrationConflictStrategy,
};

/// Create a [`IndexeddbStateStore`] and a [`IndexeddbCryptoStore`] that use the
/// same name and passphrase.
#[cfg(all(feature = "e2e-encryption", feature = "state-store"))]
pub async fn open_stores_with_name(
    name: &str,
    passphrase: Option<&str>,
) -> Result<(IndexeddbStateStore, IndexeddbCryptoStore), OpenStoreError> {
    let mut builder = IndexeddbStateStore::builder().name(name.to_owned());
    if let Some(passphrase) = passphrase {
        builder = builder.passphrase(passphrase.to_owned());
    }

    let state_store = builder.build().await.map_err(StoreError::from)?;
    let crypto_store =
        IndexeddbCryptoStore::open_with_store_cipher(name, state_store.store_cipher.clone())
            .await?;

    Ok((state_store, crypto_store))
}

/// Create an [`IndexeddbStateStore`].
///
/// If a `passphrase` is given, the store will be encrypted using a key derived
/// from that passphrase.
#[cfg(feature = "state-store")]
pub async fn open_state_store(
    name: &str,
    passphrase: Option<&str>,
) -> Result<IndexeddbStateStore, OpenStoreError> {
    let mut builder = IndexeddbStateStore::builder().name(name.to_owned());
    if let Some(passphrase) = passphrase {
        builder = builder.passphrase(passphrase.to_owned());
    }
    let state_store = builder.build().await.map_err(StoreError::from)?;

    Ok(state_store)
}

/// All the errors that can occur when opening an IndexedDB store.
#[derive(Error, Debug)]
pub enum OpenStoreError {
    /// An error occurred with the state store implementation.
    #[cfg(feature = "state-store")]
    #[error(transparent)]
    State(#[from] StoreError),

    /// An error occurred with the crypto store implementation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    Crypto(#[from] IndexeddbCryptoStoreError),
}
