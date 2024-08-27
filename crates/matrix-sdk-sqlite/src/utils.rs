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

use core::fmt;
use std::{borrow::Borrow, cmp::min, iter, ops::Deref};

use async_trait::async_trait;
use deadpool_sqlite::Object as SqliteAsyncConn;
use itertools::Itertools;
use matrix_sdk_store_encryption::StoreCipher;
use rusqlite::{limits::Limit, OptionalExtension, Params, Row, Statement, Transaction};

use crate::{
    error::{Error, Result},
    OpenStoreError,
};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Key {
    Plain(Vec<u8>),
    Hashed([u8; 32]),
}

impl Deref for Key {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Key::Plain(slice) => slice,
            Key::Hashed(bytes) => bytes,
        }
    }
}

impl Borrow<[u8]> for Key {
    fn borrow(&self) -> &[u8] {
        self.deref()
    }
}

impl rusqlite::ToSql for Key {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.deref().to_sql()
    }
}

#[async_trait]
pub(crate) trait SqliteAsyncConnExt {
    async fn execute<P>(
        &self,
        sql: impl AsRef<str> + Send + 'static,
        params: P,
    ) -> rusqlite::Result<usize>
    where
        P: Params + Send + 'static;

    async fn execute_batch(&self, sql: impl AsRef<str> + Send + 'static) -> rusqlite::Result<()>;

    async fn prepare<T, F>(
        &self,
        sql: impl AsRef<str> + Send + 'static,
        f: F,
    ) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Statement<'_>) -> rusqlite::Result<T> + Send + 'static;

    async fn query_row<T, P, F>(
        &self,
        sql: impl AsRef<str> + Send + 'static,
        params: P,
        f: F,
    ) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        P: Params + Send + 'static,
        F: FnOnce(&Row<'_>) -> rusqlite::Result<T> + Send + 'static;

    async fn with_transaction<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<rusqlite::Error> + Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, E> + Send + 'static;

    async fn chunk_large_query_over<Query, Res>(
        &self,
        mut keys_to_chunk: Vec<Key>,
        result_capacity: Option<usize>,
        do_query: Query,
    ) -> Result<Vec<Res>>
    where
        Res: Send + 'static,
        Query: Fn(&Transaction<'_>, Vec<Key>) -> Result<Vec<Res>> + Send + 'static;
}

#[async_trait]
impl SqliteAsyncConnExt for SqliteAsyncConn {
    async fn execute<P>(
        &self,
        sql: impl AsRef<str> + Send + 'static,
        params: P,
    ) -> rusqlite::Result<usize>
    where
        P: Params + Send + 'static,
    {
        self.interact(move |conn| conn.execute(sql.as_ref(), params)).await.unwrap()
    }

    async fn execute_batch(&self, sql: impl AsRef<str> + Send + 'static) -> rusqlite::Result<()> {
        self.interact(move |conn| conn.execute_batch(sql.as_ref())).await.unwrap()
    }

    async fn prepare<T, F>(
        &self,
        sql: impl AsRef<str> + Send + 'static,
        f: F,
    ) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Statement<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        self.interact(move |conn| f(conn.prepare(sql.as_ref())?)).await.unwrap()
    }

    async fn query_row<T, P, F>(
        &self,
        sql: impl AsRef<str> + Send + 'static,
        params: P,
        f: F,
    ) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        P: Params + Send + 'static,
        F: FnOnce(&Row<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        self.interact(move |conn| conn.query_row(sql.as_ref(), params, f)).await.unwrap()
    }

    async fn with_transaction<T, E, F>(&self, f: F) -> Result<T, E>
    where
        T: Send + 'static,
        E: From<rusqlite::Error> + Send + 'static,
        F: FnOnce(&Transaction<'_>) -> Result<T, E> + Send + 'static,
    {
        self.interact(move |conn| {
            let txn = conn.transaction()?;
            let result = f(&txn)?;
            txn.commit()?;
            Ok(result)
        })
        .await
        .unwrap()
    }

    /// Chunk a large query over some keys.
    ///
    /// Imagine there is a _dynamic_ query that runs potentially large number of
    /// parameters, so much that the maximum number of parameters can be hit.
    /// Then, this helper is for you. It will execute the query on chunks of
    /// parameters.
    async fn chunk_large_query_over<Query, Res>(
        &self,
        keys_to_chunk: Vec<Key>,
        result_capacity: Option<usize>,
        do_query: Query,
    ) -> Result<Vec<Res>>
    where
        Res: Send + 'static,
        Query: Fn(&Transaction<'_>, Vec<Key>) -> Result<Vec<Res>> + Send + 'static,
    {
        self.with_transaction(move |txn| {
            txn.chunk_large_query_over(keys_to_chunk, result_capacity, do_query)
        })
        .await
    }
}

pub(crate) trait SqliteTransactionExt {
    fn chunk_large_query_over<Query, Res>(
        &self,
        keys_to_chunk: Vec<Key>,
        result_capacity: Option<usize>,
        do_query: Query,
    ) -> Result<Vec<Res>>
    where
        Res: Send + 'static,
        Query: Fn(&Transaction<'_>, Vec<Key>) -> Result<Vec<Res>> + Send + 'static;
}

impl<'a> SqliteTransactionExt for Transaction<'a> {
    fn chunk_large_query_over<Query, Res>(
        &self,
        mut keys_to_chunk: Vec<Key>,
        result_capacity: Option<usize>,
        do_query: Query,
    ) -> Result<Vec<Res>>
    where
        Res: Send + 'static,
        Query: Fn(&Transaction<'_>, Vec<Key>) -> Result<Vec<Res>> + Send + 'static,
    {
        // Divide by 2 to allow space for more static parameters (not part of
        // `keys_to_chunk`).
        let maximum_chunk_size = self.limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER) / 2;
        let maximum_chunk_size: usize = maximum_chunk_size
            .try_into()
            .map_err(|_| Error::SqliteMaximumVariableNumber(maximum_chunk_size))?;

        if keys_to_chunk.len() < maximum_chunk_size {
            // Chunking isn't necessary.
            let chunk = keys_to_chunk;

            Ok(do_query(self, chunk)?)
        } else {
            // Chunking _is_ necessary.

            // Define the accumulator.
            let capacity = result_capacity.unwrap_or_default();
            let mut all_results = Vec::with_capacity(capacity);

            while !keys_to_chunk.is_empty() {
                // Chunk and run the query.
                let tail = keys_to_chunk.split_off(min(keys_to_chunk.len(), maximum_chunk_size));
                let chunk = keys_to_chunk;
                keys_to_chunk = tail;

                all_results.extend(do_query(self, chunk)?);
            }

            Ok(all_results)
        }
    }
}

/// Extension trait for a [`rusqlite::Connection`] that contains a key-value
/// table named `kv`.
///
/// The table should be created like this:
///
/// ```sql
/// CREATE TABLE "kv" (
///     "key" TEXT PRIMARY KEY NOT NULL,
///     "value" BLOB NOT NULL
/// );
/// ```
pub(crate) trait SqliteKeyValueStoreConnExt {
    /// Store the given value for the given key.
    fn set_kv(&self, key: &str, value: &[u8]) -> rusqlite::Result<()>;

    /// Set the version of the database.
    fn set_db_version(&self, version: u8) -> rusqlite::Result<()> {
        self.set_kv("version", &[version])
    }
}

impl SqliteKeyValueStoreConnExt for rusqlite::Connection {
    fn set_kv(&self, key: &str, value: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO kv VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
            (key, value),
        )?;
        Ok(())
    }
}

/// Extension trait for an [`SqliteAsyncConn`] that contains a key-value
/// table named `kv`.
///
/// The table should be created like this:
///
/// ```sql
/// CREATE TABLE "kv" (
///     "key" TEXT PRIMARY KEY NOT NULL,
///     "value" BLOB NOT NULL
/// );
/// ```
#[async_trait]
pub(crate) trait SqliteKeyValueStoreAsyncConnExt: SqliteAsyncConnExt {
    /// Whether the `kv` table exists in this database.
    async fn kv_table_exists(&self) -> rusqlite::Result<bool> {
        self.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'kv')",
            (),
            |row| row.get(0),
        )
        .await
    }

    /// Get the stored value for the given key.
    async fn get_kv(&self, key: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        let key = key.to_owned();
        self.query_row("SELECT value FROM kv WHERE key = ?", (key,), |row| row.get(0))
            .await
            .optional()
    }

    /// Store the given value for the given key.
    async fn set_kv(&self, key: &str, value: Vec<u8>) -> rusqlite::Result<()>;

    /// Get the version of the database.
    async fn db_version(&self) -> Result<u8, OpenStoreError> {
        let kv_exists = self.kv_table_exists().await.map_err(OpenStoreError::LoadVersion)?;

        if kv_exists {
            match self.get_kv("version").await.map_err(OpenStoreError::LoadVersion)?.as_deref() {
                Some([v]) => Ok(*v),
                Some(_) => Err(OpenStoreError::InvalidVersion),
                None => Err(OpenStoreError::MissingVersion),
            }
        } else {
            Ok(0)
        }
    }

    /// Get the [`StoreCipher`] of the database or create it.
    async fn get_or_create_store_cipher(
        &self,
        passphrase: &str,
    ) -> Result<StoreCipher, OpenStoreError> {
        let encrypted_cipher = self.get_kv("cipher").await.map_err(OpenStoreError::LoadCipher)?;

        let cipher = if let Some(encrypted) = encrypted_cipher {
            StoreCipher::import(passphrase, &encrypted)?
        } else {
            let cipher = StoreCipher::new()?;
            #[cfg(not(test))]
            let export = cipher.export(passphrase);
            #[cfg(test)]
            let export = cipher._insecure_export_fast_for_testing(passphrase);
            self.set_kv("cipher", export?).await.map_err(OpenStoreError::SaveCipher)?;
            cipher
        };

        Ok(cipher)
    }
}

#[async_trait]
impl SqliteKeyValueStoreAsyncConnExt for SqliteAsyncConn {
    async fn set_kv(&self, key: &str, value: Vec<u8>) -> rusqlite::Result<()> {
        let key = key.to_owned();
        self.interact(move |conn| conn.set_kv(&key, &value)).await.unwrap()?;

        Ok(())
    }
}

/// Repeat `?` n times, where n is defined by `count`. `?` are comma-separated.
pub(crate) fn repeat_vars(count: usize) -> impl fmt::Display {
    assert_ne!(count, 0, "Can't generate zero repeated vars");

    iter::repeat("?").take(count).format(",")
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn can_generate_repeated_vars() {
        assert_eq!(repeat_vars(1).to_string(), "?");
        assert_eq!(repeat_vars(2).to_string(), "?,?");
        assert_eq!(repeat_vars(5).to_string(), "?,?,?,?,?");
    }

    #[test]
    #[should_panic(expected = "Can't generate zero repeated vars")]
    fn generating_zero_vars_panics() {
        repeat_vars(0);
    }
}
