// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use deadpool_sqlite::{CreatePoolError, PoolError};
#[cfg(feature = "crypto-store")]
use matrix_sdk_crypto::CryptoStoreError;
use thiserror::Error;
use tokio::io;

/// All the errors that can occur when opening a sled store.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum OpenStoreError {
    /// Failed to create the DB's parent directory.
    #[error("Failed to create the database's parent directory")]
    CreateDir(#[source] io::Error),

    /// Failed to create the DB pool.
    #[error(transparent)]
    CreatePool(#[from] CreatePoolError),

    /// Failed to apply migrations.
    #[error("Failed to run migrations")]
    Migration(#[source] rusqlite::Error),

    /// Failed to get a DB connection from the pool.
    #[error(transparent)]
    Pool(#[from] PoolError),

    /// Failed to initialize the store cipher.
    #[error("Failed to initialize the store cipher")]
    InitCipher(#[from] matrix_sdk_store_encryption::Error),

    /// Failed to load the store cipher from the DB.
    #[error("Failed to load the store cipher from the DB")]
    LoadCipher(#[source] rusqlite::Error),

    /// Failed to save the store cipher to the DB.
    #[error("Failed to save the store cipher to the DB")]
    SaveCipher(#[source] rusqlite::Error),
}

#[derive(Debug)]
pub(crate) enum Error {
    #[cfg(feature = "crypto-store")]
    Crypto(CryptoStoreError),
    Sqlite(rusqlite::Error),
    Pool(PoolError),
}

#[cfg(feature = "crypto-store")]
impl From<CryptoStoreError> for Error {
    fn from(value: CryptoStoreError) -> Self {
        Self::Crypto(value)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sqlite(value)
    }
}

impl From<PoolError> for Error {
    fn from(value: PoolError) -> Self {
        Self::Pool(value)
    }
}

#[cfg(feature = "crypto-store")]
impl From<Error> for CryptoStoreError {
    fn from(value: Error) -> Self {
        match value {
            Error::Crypto(c) => c,
            Error::Sqlite(b) => CryptoStoreError::backend(b),
            Error::Pool(b) => CryptoStoreError::backend(b),
        }
    }
}

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;
