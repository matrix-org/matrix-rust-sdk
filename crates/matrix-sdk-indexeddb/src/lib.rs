#![cfg_attr(not(target_family = "wasm"), allow(unused))]

#[cfg(feature = "state-store")]
use matrix_sdk_base::store::StoreError;
use thiserror::Error;

#[cfg(feature = "e2e-encryption")]
mod crypto_store;
mod error;
#[cfg(feature = "event-cache-store")]
mod event_cache_store;
#[cfg(feature = "media-store")]
mod media_store;
mod serializer;
#[cfg(feature = "state-store")]
mod state_store;
#[cfg(any(feature = "event-cache-store", feature = "media-store"))]
mod transaction;

#[cfg(feature = "e2e-encryption")]
pub use crypto_store::{IndexeddbCryptoStore, IndexeddbCryptoStoreError};
#[cfg(feature = "state-store")]
pub use state_store::{
    IndexeddbStateStore, IndexeddbStateStoreBuilder, IndexeddbStateStoreError,
    MigrationConflictStrategy,
};

#[cfg(feature = "event-cache-store")]
pub use crate::event_cache_store::{
    IndexeddbEventCacheStore, IndexeddbEventCacheStoreBuilder, IndexeddbEventCacheStoreError,
};
#[cfg(feature = "media-store")]
pub use crate::media_store::{
    IndexeddbMediaStore, IndexeddbMediaStoreBuilder, IndexeddbMediaStoreError,
};

/// Structure containing implementations of every type
/// of store using IndexedDB for persistent storage.
///
/// Note that each of the stores is behind a feature flag and will
/// only be available when its corresponding flag is set.
pub struct IndexeddbStores {
    /// An IndexedDB-backed implementation of [`CryptoStore`][1]
    ///
    /// [1]: matrix_sdk_crypto::store::CryptoStore
    #[cfg(feature = "e2e-encryption")]
    pub crypto: IndexeddbCryptoStore,
    /// An IndexedDB-backed implementation of [`StateStore`][1]
    ///
    /// [1]: matrix_sdk_base::store::StateStore
    #[cfg(feature = "state-store")]
    pub state: IndexeddbStateStore,
    /// An IndexedDB-backed implementation of [`EventCacheStore`][1]
    ///
    /// [1]: matrix_sdk_base::event_cache::store::EventCacheStore
    #[cfg(feature = "event-cache-store")]
    pub event_cache: IndexeddbEventCacheStore,
    /// An IndexedDB-backed implementation of [`MediaStore`][1]
    ///
    /// [1]: matrix_sdk_base::media::store::MediaStore
    #[cfg(feature = "media-store")]
    pub media: IndexeddbMediaStore,
}

impl IndexeddbStores {
    /// Opens and returns all stores using the given database name and,
    /// optionally, a passphrase.
    ///
    /// If `e2e-encryption` and `state-store` features are not enabled,
    /// `passphrase` is ignored. Otherwise `passphrase` is used to import or
    /// create a [`StoreCipher`][1] which encrypts contents of all stores.
    ///
    /// Note that each of the stores is behind a feature flag and will only be
    /// opened when the corresponding flag is set.
    ///
    /// [1]: matrix_sdk_store_encryption::StoreCipher
    pub async fn open(
        name: &str,
        #[allow(unused_variables)] passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        #[cfg(all(feature = "e2e-encryption", feature = "state-store"))]
        if let Some(passphrase) = passphrase {
            return Self::open_with_passphrase(name, passphrase).await;
        }

        Self::open_without_passphrase(name).await
    }

    /// Opens and returns all stores using the given database name. Contents of
    /// the stores are NOT encrypted.
    ///
    /// Note that each of the stores is behind a feature flag and will only be
    /// opened when the corresponding flag is set.
    #[allow(clippy::unused_async)]
    pub async fn open_without_passphrase(name: &str) -> Result<Self, OpenStoreError> {
        #[cfg(feature = "state-store")]
        let state = IndexeddbStateStore::builder()
            .name(name.to_owned())
            .build()
            .await
            .map_err(StoreError::from)?;

        #[cfg(feature = "e2e-encryption")]
        let crypto = IndexeddbCryptoStore::open_with_name(name).await?;

        #[cfg(feature = "event-cache-store")]
        let event_cache = IndexeddbEventCacheStoreBuilder::with_prefix(name).build().await?;

        #[cfg(feature = "media-store")]
        let media = IndexeddbMediaStoreBuilder::with_prefix(name).build().await?;

        Ok(Self {
            #[cfg(feature = "state-store")]
            state,
            #[cfg(feature = "e2e-encryption")]
            crypto,
            #[cfg(feature = "event-cache-store")]
            event_cache,
            #[cfg(feature = "media-store")]
            media,
        })
    }

    /// Opens and returns all stores using the given database name and
    /// passphrase. Passphrase is used to import or create a [`StoreCipher`][1]
    /// which encrypts contents of all stores.
    ///
    /// Note that [`IndexeddbEventCacheStore`] and [`IndexeddbMediaStore`] are
    /// behind feature flags and will only be opened when their
    /// corresponding flags are set.
    ///
    /// [1]: matrix_sdk_store_encryption::StoreCipher
    #[cfg(all(feature = "e2e-encryption", feature = "state-store"))]
    pub async fn open_with_passphrase(
        name: &str,
        passphrase: &str,
    ) -> Result<Self, OpenStoreError> {
        let state = IndexeddbStateStore::builder()
            .name(name.to_owned())
            .passphrase(passphrase.to_owned())
            .build()
            .await
            .map_err(StoreError::from)?;
        let store_cipher =
            state.store_cipher.clone().ok_or(OpenStoreError::FailedToLoadStoreCipher)?;

        let crypto =
            IndexeddbCryptoStore::open_with_store_cipher(name, Some(store_cipher.clone())).await?;

        #[cfg(feature = "event-cache-store")]
        let event_cache = IndexeddbEventCacheStoreBuilder::with_prefix(name)
            .store_cipher(store_cipher.clone())
            .build()
            .await?;

        #[cfg(feature = "media-store")]
        let media = IndexeddbMediaStoreBuilder::with_prefix(name)
            .store_cipher(store_cipher.clone())
            .build()
            .await?;

        Ok(Self {
            state,
            crypto,
            #[cfg(feature = "event-cache-store")]
            event_cache,
            #[cfg(feature = "media-store")]
            media,
        })
    }
}

/// Create a [`IndexeddbStateStore`] and a [`IndexeddbCryptoStore`] that use the
/// same name and passphrase.
#[deprecated(
    note = "this function only opens state and crypto stores, use `IndexeddbStores::open()` instead."
)]
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

    /// An error occurred while trying to load a [`StoreCipher`][1] from a
    /// passphrase
    ///
    /// [1]: matrix_sdk_store_encryption::StoreCipher
    #[cfg(feature = "e2e-encryption")]
    #[error("failed to load store cipher")]
    FailedToLoadStoreCipher,

    /// An error occurred with the event cache store implementation.
    #[cfg(feature = "event-cache-store")]
    #[error(transparent)]
    Event(#[from] IndexeddbEventCacheStoreError),

    /// An error occurred with the media store implementation.
    #[cfg(feature = "media-store")]
    #[error(transparent)]
    Media(#[from] IndexeddbMediaStoreError),
}
