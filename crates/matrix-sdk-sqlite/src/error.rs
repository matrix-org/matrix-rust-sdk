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
#[cfg(feature = "state-store")]
use matrix_sdk_base::store::StoreError as StateStoreError;
#[cfg(feature = "crypto-store")]
use matrix_sdk_crypto::CryptoStoreError;
use thiserror::Error;
use tokio::io;

/// All the errors that can occur when opening a SQLite store.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum OpenStoreError {
    /// Failed to create the DB's parent directory.
    #[error("Failed to create the database's parent directory")]
    CreateDir(#[source] io::Error),

    /// Failed to create the DB pool.
    #[error(transparent)]
    CreatePool(#[from] CreatePoolError),

    /// Failed to load the database's version.
    #[error("Failed to load database version")]
    LoadVersion(#[source] rusqlite::Error),

    /// The version of the database is missing.
    #[error("Missing database version")]
    MissingVersion,

    /// The version of the database is invalid.
    #[error("Invalid database version")]
    InvalidVersion,

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

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(rusqlite::Error),
    #[error(transparent)]
    Pool(PoolError),
    #[error(transparent)]
    Encode(rmp_serde::encode::Error),
    #[error(transparent)]
    Decode(rmp_serde::decode::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(matrix_sdk_store_encryption::Error),
    #[error("can't save/load sessions or group sessions in the store before an account is stored")]
    AccountUnset,
    #[error(transparent)]
    Pickle(#[from] vodozemac::PickleError),
    #[error("An object failed to be decrypted while unpickling")]
    Unpickle,
    #[error("Redaction failed: {0}")]
    Redaction(#[source] ruma::canonical_json::RedactionError),
}

macro_rules! impl_from {
    ( $ty:ty => $enum:ident::$variant:ident ) => {
        impl From<$ty> for $enum {
            fn from(value: $ty) -> Self {
                Self::$variant(value)
            }
        }
    };
}

impl_from!(rusqlite::Error => Error::Sqlite);
impl_from!(PoolError => Error::Pool);
impl_from!(rmp_serde::encode::Error => Error::Encode);
impl_from!(rmp_serde::decode::Error => Error::Decode);
impl_from!(matrix_sdk_store_encryption::Error => Error::Encryption);

#[cfg(feature = "crypto-store")]
impl From<Error> for CryptoStoreError {
    fn from(e: Error) -> Self {
        CryptoStoreError::backend(e)
    }
}

#[cfg(feature = "state-store")]
impl From<Error> for StateStoreError {
    fn from(e: Error) -> Self {
        match e {
            Error::Json(e) => StateStoreError::Json(e),
            Error::Encryption(e) => StateStoreError::Encryption(e),
            Error::Redaction(e) => StateStoreError::Redaction(e),
            e => StateStoreError::backend(e),
        }
    }
}

pub(crate) type Result<T, E = Error> = std::result::Result<T, E>;
