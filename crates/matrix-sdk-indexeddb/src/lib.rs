#![cfg_attr(not(target_arch = "wasm32"), allow(unused))]

use matrix_sdk_base::store::{StoreConfig, StoreError};
use thiserror::Error;

#[cfg(feature = "e2e-encryption")]
mod crypto_store;
mod safe_encode;
mod state_store;

#[cfg(feature = "e2e-encryption")]
pub use crypto_store::{IndexeddbCryptoStore, IndexeddbCryptoStoreError};
pub use state_store::{
    IndexeddbStateStore, IndexeddbStateStoreBuilder, IndexeddbStateStoreError,
    MigrationConflictStrategy,
};

/// Create a [`IndexeddbStateStore`] and a [`IndexeddbCryptoStore`] that use the
/// same name and passphrase.
#[cfg(feature = "e2e-encryption")]
async fn open_stores_with_name(
    name: impl Into<String>,
    passphrase: Option<&str>,
) -> Result<(IndexeddbStateStore, IndexeddbCryptoStore), OpenStoreError> {
    let name = name.into();
    let mut builder = IndexeddbStateStore::builder();
    builder.name(name.clone());

    if let Some(passphrase) = passphrase {
        builder.passphrase(passphrase.to_owned());
    }

    let state_store = builder.build().await.map_err(StoreError::from)?;
    let crypto_store =
        IndexeddbCryptoStore::open_with_store_cipher(name, state_store.store_cipher.clone())
            .await?;

    Ok((state_store, crypto_store))
}

/// Create a [`StoreConfig`] with an opened indexeddb [`IndexeddbStateStore`]
/// that uses the given name and passphrase. If `encryption` is enabled, a
/// [`IndexeddbCryptoStore`] with the same parameters is also opened.
pub async fn make_store_config(
    name: impl Into<String>,
    passphrase: Option<&str>,
) -> Result<StoreConfig, OpenStoreError> {
    #[cfg(target_arch = "wasm32")]
    {
        let name = name.into();

        #[cfg(feature = "e2e-encryption")]
        {
            let (state_store, crypto_store) = open_stores_with_name(name, passphrase).await?;
            Ok(StoreConfig::new().state_store(state_store).crypto_store(crypto_store))
        }

        #[cfg(not(feature = "e2e-encryption"))]
        {
            let mut builder = IndexeddbStateStore::builder();
            builder.name(name.clone());

            if let Some(passphrase) = passphrase {
                builder.passphrase(passphrase.to_owned());
            }

            let state_store = builder.build().await.map_err(StoreError::from)?;

            Ok(StoreConfig::new().state_store(state_store))
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    panic!("the IndexedDB is only available on the 'wasm32' arch")
}

/// All the errors that can occur when opening an IndexedDB store.
#[derive(Error, Debug)]
pub enum OpenStoreError {
    /// An error occurred with the state store implementation.
    #[error(transparent)]
    State(#[from] StoreError),

    /// An error occurred with the crypto store implementation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    Crypto(#[from] IndexeddbCryptoStoreError),
}
