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

use deadpool_sqlite::CreatePoolError;
#[cfg(feature = "crypto-store")]
use matrix_sdk_crypto::CryptoStoreError;
use thiserror::Error;

/// All the errors that can occur when opening a sled store.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum OpenStoreError {
    /// An error occurred with the crypto store implementation.
    #[cfg(feature = "crypto-store")]
    #[error(transparent)]
    Crypto(#[from] CryptoStoreError),

    /// An error occurred with sqlite.
    #[error(transparent)]
    Sqlite(#[from] CreatePoolError),
}

#[derive(Debug)]
pub(crate) enum Error {
    #[cfg(feature = "crypto-store")]
    Crypto(CryptoStoreError),
    Sqlite(rusqlite::Error),
    Pool(deadpool_sqlite::PoolError),
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

impl From<deadpool_sqlite::PoolError> for Error {
    fn from(value: deadpool_sqlite::PoolError) -> Self {
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
