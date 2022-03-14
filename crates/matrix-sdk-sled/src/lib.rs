use std::path::Path;

use matrix_sdk_base::store::StoreConfig;

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
) -> Result<(Box<StateStore>, Box<CryptoStore>), anyhow::Error> {
    if let Some(passphrase) = passphrase {
        let state_store = StateStore::open_with_passphrase(path, passphrase)?;
        let crypto_store = state_store.get_crypto_store(Some(passphrase))?;
        Ok((Box::new(state_store), Box::new(crypto_store)))
    } else {
        let state_store = StateStore::open_with_path(path)?;
        let crypto_store = state_store.get_crypto_store(None)?;
        Ok((Box::new(state_store), Box::new(crypto_store)))
    }
}

/// Create a [`StoreConfig`] with an opened sled [`StateStore`] that uses the
/// given path and passphrase. If `encryption` is enabled, a [`CryptoStore`]
/// with the same parameters is also opened.
pub fn make_store_config(
    path: impl AsRef<Path>,
    passphrase: Option<&str>,
) -> Result<StoreConfig, anyhow::Error> {
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
