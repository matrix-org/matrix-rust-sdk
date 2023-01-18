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

use async_trait::async_trait;
use deadpool_sqlite::{CreatePoolError, Object as SqliteConn};
use matrix_sdk_crypto::{store::Result, CryptoStoreError};
use matrix_sdk_store_encryption::StoreCipher;
use rusqlite::OptionalExtension;
use thiserror::Error;
use tracing::{debug, error};

#[cfg(feature = "crypto-store")]
mod crypto_store;
mod utils;

pub use self::crypto_store::SqliteCryptoStore;
use self::utils::SqliteObjectExt;

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

const DATABASE_VERSION: u8 = 1;

async fn run_migrations(conn: &SqliteConn) -> Result<()> {
    let metadata_exists = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'metadata'",
            (),
            |row| row.get::<_, u32>(0),
        )
        .await
        .map_err(CryptoStoreError::backend)?
        > 0;

    let version = if metadata_exists {
        match conn.get_metadata("version").await?.as_deref() {
            Some([v]) => *v,
            Some(_) => {
                error!("version database field has multiple bytes");
                return Ok(());
            }
            None => {
                error!("version database field is missing");
                return Ok(());
            }
        }
    } else {
        0
    };

    if version == 0 {
        debug!("Creating database");
    } else if version < DATABASE_VERSION {
        debug!(version, new_version = DATABASE_VERSION, "Upgrading database");
    }

    if version < 1 {
        conn.with_transaction(|txn| txn.execute_batch(include_str!("../migrations/001_init.sql")))
            .await
            .map_err(CryptoStoreError::backend)?;
    }

    Ok(())
}

async fn get_or_create_store_cipher(passphrase: &str, conn: &SqliteConn) -> Result<StoreCipher> {
    let encrypted_cipher = conn.get_metadata("cipher").await?;

    let cipher = if let Some(encrypted) = encrypted_cipher {
        StoreCipher::import(passphrase, &encrypted)
            .map_err(|_| CryptoStoreError::UnpicklingError)?
    } else {
        let cipher = StoreCipher::new().map_err(CryptoStoreError::backend)?;
        #[cfg(not(test))]
        let export = cipher.export(passphrase);
        #[cfg(test)]
        let export = cipher._insecure_export_fast_for_testing(passphrase);
        conn.set_metadata("cipher", export.map_err(CryptoStoreError::backend)?).await?;
        cipher
    };

    Ok(cipher)
}

#[async_trait]
trait SqliteObjectStoreExt: SqliteObjectExt {
    async fn get_metadata(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let key = key.to_owned();
        self.query_row("SELECT value FROM metadata WHERE key = ?", (key,), |row| row.get(0))
            .await
            .optional()
            .map_err(CryptoStoreError::backend)
    }

    async fn set_metadata(&self, key: &str, value: Vec<u8>) -> Result<()> {
        let key = key.to_owned();
        self.execute(
            "INSERT INTO metadata VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
            (key, value),
        )
        .await
        .map_err(CryptoStoreError::backend)?;

        Ok(())
    }
}

#[async_trait]
impl SqliteObjectStoreExt for deadpool_sqlite::Object {}
