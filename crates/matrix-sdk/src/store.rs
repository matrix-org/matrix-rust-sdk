// Copyright 2022 Kévin Commaille
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Functions and types to initialize a store.
//!
//! The re-exports present here depend on the store-related features that are
//! enabled:
//!
//! 1. `sled-state-store` provides a `StateStore`, while
//! `sled-crypto-store` provides also a `CryptoStore` for encryption data. This
//! is the default persistent store implementation for non-WebAssembly.
//! 2. `indexeddb_store` provides both a `StateStore` and a `CryptoStore` if
//! `encryption` is also enabled. This is the default persistent store
//! implementation for WebAssembly.
//!
//! Both options provide a `make_store_config` convenience method to create a
//! [`StoreConfig`] for [`ClientBuilder::store_config()`].
//!
//! [`StoreConfig`]: crate::config::StoreConfig
//! [`ClientBuilder::store_config()`]: crate::ClientBuilder::store_config

#[cfg(any(feature = "indexeddb-state-store", feature = "indexeddb-crypto-store"))]
pub use matrix_sdk_indexeddb::*;
#[cfg(any(feature = "sled-state-store", feature = "sled-crypto-store"))]
pub use matrix_sdk_sled::*;

// FIXME Move these two methods back to the matrix-sdk-sled crate once weak
// dependency features are stable and we decide to bump the MSRV.

/// Create a [`StoreConfig`] with an opened sled [`StateStore`] that uses the
/// given path and passphrase. If `encryption` is enabled, a [`CryptoStore`]
/// with the same parameters is also opened.
///
/// [`StoreConfig`]: #crate::config::StoreConfig
#[cfg(any(feature = "sled-state-store", feature = "sled-crypto-store"))]
pub fn make_store_config(
    path: impl AsRef<std::path::Path>,
    passphrase: Option<&str>,
) -> Result<crate::config::StoreConfig, OpenStoreError> {
    #[cfg(all(feature = "encryption", feature = "sled-state-store"))]
    {
        let (state_store, crypto_store) = open_stores_with_path(path, passphrase)?;
        Ok(crate::config::StoreConfig::new().state_store(state_store).crypto_store(crypto_store))
    }

    #[cfg(all(feature = "encryption", not(feature = "sled-state-store")))]
    {
        let crypto_store = CryptoStore::open_with_passphrase(path, passphrase)?;
        Ok(crate::config::StoreConfig::new().crypto_store(Box::new(crypto_store)))
    }

    #[cfg(not(feature = "encryption"))]
    {
        let state_store = if let Some(passphrase) = passphrase {
            StateStore::open_with_passphrase(path, passphrase)?
        } else {
            StateStore::open_with_path(path)?
        };

        Ok(crate::config::StoreConfig::new().state_store(Box::new(state_store)))
    }
}

/// Create a [`StateStore`] and a [`CryptoStore`] that use the same database and
/// passphrase.
#[cfg(all(feature = "sled-state-store", feature = "sled-crypto-store"))]
fn open_stores_with_path(
    path: impl AsRef<std::path::Path>,
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
