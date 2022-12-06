// Copyright 2022 The Matrix.org Foundation C.I.C.
//
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

#[cfg(all(test, feature = "crypto-store"))]
mod fake_redis;
#[cfg(feature = "crypto-store")]
mod real_redis;
#[cfg(feature = "crypto-store")]
mod redis_crypto_store;
#[cfg(feature = "crypto-store")]
mod redis_shim;

#[cfg(any(feature = "state-store", feature = "crypto-store"))]
use matrix_sdk_base::store::StoreConfig;
#[cfg(feature = "state-store")]
use matrix_sdk_base::store::StoreError;
#[cfg(feature = "crypto-store")]
use matrix_sdk_crypto::store::CryptoStoreError;
use redis::RedisError;
#[cfg(feature = "crypto-store")]
pub use redis_crypto_store::RedisStore as CryptoStore;
#[cfg(feature = "crypto-store")]
use redis_crypto_store::RedisStore;
use thiserror::Error;

/// All the errors that can occur when opening a redis store.
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

    /// An error occurred with redis.
    #[error(transparent)]
    Redis(#[from] RedisError),
}

/// Create a [`StoreConfig`].
///
/// If the `e2e-encryption` Cargo feature is enabled, a [`CryptoStore`] is opened.
///
/// [`StoreConfig`]: #StoreConfig
#[cfg(any(feature = "state-store", feature = "crypto-store"))]
pub async fn make_store_config(
    redis_url: &str,
    passphrase: Option<&str>,
    redis_prefix: &str,
) -> Result<StoreConfig, OpenStoreError> {
    use real_redis::RealRedisClient;

    #[cfg(all(feature = "crypto-store", feature = "state-store"))]
    {
        panic!("Currently don't have a Redis state store!");

        let underlying_client = redis::Client::open(redis_url).unwrap();
        let client = RealRedisClient::from(underlying_client);
        let crypto_store = RedisStore::open(client, passphrase, String::from(redis_prefix)).await?;
        // TODO: state_store
        Ok(StoreConfig::new().state_store(state_store).crypto_store(crypto_store))
    }

    #[cfg(all(feature = "crypto-store", not(feature = "state-store")))]
    {
        let underlying_client = redis::Client::open(redis_url).unwrap();
        let client = RealRedisClient::from(underlying_client);
        let crypto_store = RedisStore::open(client, passphrase, String::from(redis_prefix)).await?;
        Ok(StoreConfig::new().crypto_store(crypto_store))
    }

    #[cfg(not(feature = "crypto-store"))]
    {
        panic!("Currently don't have a Redis state store!");

        let mut store_builder = RedisStateStore::builder();
        store_builder.path(path.as_ref().to_path_buf());

        if let Some(passphrase) = passphrase {
            store_builder.passphrase(passphrase.to_owned());
        };
        let state_store = store_builder.build().map_err(StoreError::backend)?;

        Ok(StoreConfig::new().state_store(state_store))
    }
}
