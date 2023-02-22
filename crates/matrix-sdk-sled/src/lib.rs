#[cfg(any(feature = "state-store", feature = "crypto-store"))]
use matrix_sdk_base::store::StoreConfig;
#[cfg(feature = "state-store")]
use matrix_sdk_base::store::StoreError;
#[cfg(feature = "crypto-store")]
use matrix_sdk_crypto::store::CryptoStoreError;
use sled::Error as SledError;
use thiserror::Error;

#[cfg(feature = "crypto-store")]
mod crypto_store;
mod encode_key;
#[cfg(feature = "state-store")]
mod state_store;

#[cfg(feature = "crypto-store")]
pub use crypto_store::SledCryptoStore;
#[cfg(feature = "state-store")]
pub use state_store::{MigrationConflictStrategy, SledStateStore, SledStateStoreBuilder};

/// All the errors that can occur when opening a sled store.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum OpenStoreError {
    /// An error occurred with the state store implementation.
    #[cfg(feature = "state-store")]
    #[error(transparent)]
    State(#[from] StoreError),

    /// An error occurred with the crypto store implementation.
    #[cfg(feature = "crypto-store")]
    #[error(transparent)]
    Crypto(#[from] CryptoStoreError),

    /// An error occurred with sled.
    #[error(transparent)]
    Sled(#[from] SledError),
}

/// Create a [`StoreConfig`] with an opened [`SledStateStore`] that uses the
/// given path and passphrase.
///
/// If the `e2e-encryption` Cargo feature is enabled, a [`SledCryptoStore`] with
/// the same parameters is also opened.
///
/// [`StoreConfig`]: #StoreConfig
#[cfg(any(feature = "state-store", feature = "crypto-store"))]
pub async fn make_store_config(
    path: impl AsRef<std::path::Path>,
    passphrase: Option<&str>,
) -> Result<StoreConfig, OpenStoreError> {
    #[cfg(all(feature = "crypto-store", feature = "state-store"))]
    {
        let (state_store, crypto_store) = open_stores_with_path(path, passphrase).await?;
        Ok(StoreConfig::new().state_store(state_store).crypto_store(crypto_store))
    }

    #[cfg(all(feature = "crypto-store", not(feature = "state-store")))]
    {
        let crypto_store = SledCryptoStore::open(path, passphrase).await?;
        Ok(StoreConfig::new().crypto_store(crypto_store))
    }

    #[cfg(not(feature = "crypto-store"))]
    {
        let mut store_builder = SledStateStore::builder().path(path.as_ref().to_path_buf());

        if let Some(passphrase) = passphrase {
            store_builder = store_builder.passphrase(passphrase.to_owned());
        }
        let state_store = store_builder.build().map_err(StoreError::backend)?;

        Ok(StoreConfig::new().state_store(state_store))
    }
}

/// Create a [`StateStore`] and a [`CryptoStore`] that use the same database and
/// passphrase.
#[cfg(all(feature = "state-store", feature = "crypto-store"))]
async fn open_stores_with_path(
    path: impl AsRef<std::path::Path>,
    passphrase: Option<&str>,
) -> Result<(SledStateStore, SledCryptoStore), OpenStoreError> {
    let mut store_builder = SledStateStore::builder().path(path.as_ref().to_path_buf());
    if let Some(passphrase) = passphrase {
        store_builder = store_builder.passphrase(passphrase.to_owned());
    }

    let state_store = store_builder.build().map_err(StoreError::backend)?;
    let crypto_store = state_store.open_crypto_store().await?;
    Ok((state_store, crypto_store))
}
