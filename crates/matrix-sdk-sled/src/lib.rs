#[cfg(feature = "state-store")]
use matrix_sdk_base::store::StoreError;
#[cfg(feature = "crypto-store")]
use matrix_sdk_crypto::store::CryptoStoreError;
use sled::Error as SledError;
use thiserror::Error;

#[cfg(feature = "crypto-store")]
mod cryptostore;
mod encode_key;
#[cfg(feature = "state-store")]
mod state_store;

#[cfg(feature = "crypto-store")]
pub use cryptostore::SledStore as CryptoStore;
#[cfg(feature = "state-store")]
pub use state_store::SledStore as StateStore;

/// All the errors that can occur when opening a sled store.
#[derive(Error, Debug)]
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
