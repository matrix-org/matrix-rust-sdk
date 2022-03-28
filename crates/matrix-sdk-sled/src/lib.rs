use std::path::Path;

use matrix_sdk_base::store::{StoreConfig, StoreError};
#[cfg(feature = "encryption")]
use matrix_sdk_crypto::store::CryptoStoreError;
use sled::Error as SledError;
use thiserror::Error;

#[cfg(feature = "encryption")]
mod cryptostore;
mod state_store;

#[cfg(feature = "encryption")]
pub use cryptostore::SledStore as CryptoStore;
pub use state_store::SledStore as StateStore;

#[cfg(feature = "encryption")]
/// Create a [`StateStore`] and a [`CryptoStore`] that use the same database and
/// passphrase.
fn open_stores_with_path(
    path: impl AsRef<Path>,
    passphrase: Option<&str>,
) -> Result<(Box<StateStore>, Box<CryptoStore>), OpenStoreError> {
    if let Some(passphrase) = passphrase {
        let state_store = StateStore::open_with_passphrase(path, passphrase)?;
        let crypto_store = state_store.open_crypto_store(Some(passphrase))?;
        Ok((Box::new(state_store), Box::new(crypto_store)))
    } else {
        let state_store = StateStore::open_with_path(path)?;
        let crypto_store = state_store.open_crypto_store(None)?;
        Ok((Box::new(state_store), Box::new(crypto_store)))
    }
}

/// Create a [`StoreConfig`] with an opened sled [`StateStore`] that uses the
/// given path and passphrase. If `encryption` is enabled, a [`CryptoStore`]
/// with the same parameters is also opened.
pub fn make_store_config(
    path: impl AsRef<Path>,
    passphrase: Option<&str>,
) -> Result<StoreConfig, OpenStoreError> {
    #[cfg(feature = "encryption")]
    {
        let (state_store, crypto_store) = open_stores_with_path(path, passphrase)?;
        Ok(StoreConfig::new().state_store(state_store).crypto_store(crypto_store))
    }

    #[cfg(not(feature = "encryption"))]
    {
        let state_store = if let Some(passphrase) = passphrase {
            StateStore::open_with_passphrase(path, passphrase)?
        } else {
            StateStore::open_with_path(path)?
        };

        Ok(StoreConfig::new().state_store(Box::new(state_store)))
    }
}

/// All the errors that can occur when opening a sled store.
#[derive(Error, Debug)]
pub enum OpenStoreError {
    /// An error occurred with the state store implementation.
    #[error(transparent)]
    State(#[from] StoreError),

    /// An error occurred with the crypto store implementation.
    #[cfg(feature = "encryption")]
    #[error(transparent)]
    Crypto(#[from] CryptoStoreError),

    /// An error occurred with sled.
    #[error(transparent)]
    Sled(#[from] SledError),
}
