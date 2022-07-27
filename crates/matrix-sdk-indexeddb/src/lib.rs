#[cfg(target_arch = "wasm32")]
use matrix_sdk_base::store::{StoreConfig, StoreError};
#[cfg(target_arch = "wasm32")]
use thiserror::Error;

mod safe_encode;

#[cfg(target_arch = "wasm32")]
mod state_store;

#[cfg(target_arch = "wasm32")]
#[cfg(feature = "e2e-encryption")]
mod cryptostore;

#[cfg(target_arch = "wasm32")]
#[cfg(feature = "e2e-encryption")]
pub use cryptostore::IndexeddbStore as CryptoStore;
#[cfg(target_arch = "wasm32")]
#[cfg(feature = "e2e-encryption")]
use cryptostore::IndexeddbStoreError;
#[cfg(target_arch = "wasm32")]
pub use state_store::{IndexeddbStore as StateStore, IndexeddbStoreBuilder as StateStoreBuilder};

#[cfg(target_arch = "wasm32")]
#[cfg(feature = "e2e-encryption")]
/// Create a [`StateStore`] and a [`CryptoStore`] that use the same name and
/// passphrase.
async fn open_stores_with_name(
    name: impl Into<String>,
    passphrase: Option<&str>,
) -> Result<(StateStore, CryptoStore), OpenStoreError> {
    let name = name.into();
    let mut builder = StateStore::builder();
    builder.name(name.clone());

    if let Some(passphrase) = passphrase {
        builder.passphrase(passphrase.to_owned());
    }

    let state_store = builder.build().await.map_err(StoreError::from)?;
    let crypto_store =
        CryptoStore::open_with_store_cipher(name, state_store.store_cipher.clone()).await?;

    Ok((state_store, crypto_store))
}

#[cfg(target_arch = "wasm32")]
/// Create a [`StoreConfig`] with an opened indexeddb [`StateStore`] that uses
/// the given name and passphrase. If `encryption` is enabled, a [`CryptoStore`]
/// with the same parameters is also opened.
pub async fn make_store_config(
    name: impl Into<String>,
    passphrase: Option<&str>,
) -> Result<StoreConfig, OpenStoreError> {
    let name = name.into();

    #[cfg(feature = "e2e-encryption")]
    {
        let (state_store, crypto_store) = open_stores_with_name(name, passphrase).await?;
        Ok(StoreConfig::new().state_store(state_store).crypto_store(crypto_store))
    }

    #[cfg(not(feature = "e2e-encryption"))]
    {
        let mut builder = StateStore::builder();
        builder.name(name.clone());

        if let Some(passphrase) = passphrase {
            builder.passphrase(passphrase.to_owned());
        }

        let state_store = builder.build().await.map_err(StoreError::from)?;

        Ok(StoreConfig::new().state_store(state_store))
    }
}

/// All the errors that can occur when opening an IndexedDB store.
#[cfg(target_arch = "wasm32")]
#[derive(Error, Debug)]
pub enum OpenStoreError {
    /// An error occurred with the state store implementation.
    #[error(transparent)]
    State(#[from] StoreError),

    /// An error occurred with the crypto store implementation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    Crypto(#[from] IndexeddbStoreError),
}
