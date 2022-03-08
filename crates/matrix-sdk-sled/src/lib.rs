#[cfg(feature = "encryption")]
use std::path::Path;

#[cfg(feature = "encryption")]
mod cryptostore;
mod state_store;

#[cfg(feature = "encryption")]
pub use cryptostore::SledStore as CryptoStore;
pub use state_store::SledStore as StateStore;

#[cfg(feature = "encryption")]
/// Create a [`StateStore`] and a [`CryptoStore`] that use the same database and
/// passphrase.
pub fn open_stores_with_path(
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
