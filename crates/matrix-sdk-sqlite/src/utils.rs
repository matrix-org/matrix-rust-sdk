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
use ruma::time::SystemTime;
use rusqlite::{limits::Limit, OptionalExtension, Params, Row, Statement, Transaction};
use serde::{de::DeserializeOwned, Serialize};

use crate::{
    error::{Error, Result},
    OpenStoreError, RuntimeConfig,
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

    /// Apply the [`RuntimeConfig`].
    ///
    /// It will call the `Self::optimize`, `Self::cache_size` or
    /// `Self::journal_size_limit` methods automatically based on the
    /// `RuntimeConfig` values.
    ///
    /// It is possible to call these methods individually though. This
    /// `apply_runtime_config` method allows to automate this process.
    async fn apply_runtime_config(&self, runtime_config: RuntimeConfig) -> Result<()> {
        let RuntimeConfig { optimize, cache_size, journal_size_limit } = runtime_config;

        if optimize {
            self.optimize().await?;
        }

        self.cache_size(cache_size).await?;
        self.journal_size_limit(journal_size_limit).await?;

        Ok(())
    }

    /// Optimize the database.
    ///
    /// The SQLite documentation recommends to run this regularly and after any
    /// schema change. The easiest is to do it consistently when the store is
    /// constructed, after eventual migrations.
    ///
    /// See [`PRAGMA optimize`] to learn more.
    ///
    /// [`PRAGMA cache_size`]: https://www.sqlite.org/pragma.html#pragma_optimize
    async fn optimize(&self) -> Result<()> {
        self.execute_batch("PRAGMA optimize = 0x10002;").await?;
        Ok(())
    }

    /// Define the maximum size in **bytes** the SQLite cache can use.
    ///
    /// See [`PRAGMA cache_size`] to learn more.
    ///
    /// [`PRAGMA cache_size`]: https://www.sqlite.org/pragma.html#pragma_cache_size
    async fn cache_size(&self, cache_size: u32) -> Result<()> {
        // `N` in `PRAGMA cache_size = -N` is expressed in kibibytes.
        // `cache_size` is expressed in bytes. Let's convert.
        let n = cache_size / 1024;

        self.execute_batch(format!("PRAGMA cache_size = -{n};")).await?;
        Ok(())
    }

    /// Limit the size of the WAL file, in **bytes**.
    ///
    /// By default, while the DB connections of the databases are open, [the
    /// size of the WAL file can keep increasing][size_wal_file] depending on
    /// the size needed for the transactions. A critical case is `VACUUM`
    /// which basically writes the content of the DB file to the WAL file
    /// before writing it back to the DB file, so we end up taking twice the
    /// size of the database.
    ///
    /// By setting this limit, the WAL file is truncated after its content is
    /// written to the database, if it is bigger than the limit.
    ///
    /// See [`PRAGMA journal_size_limit`] to learn more. The value `limit`
    /// corresponds to `N` in `PRAGMA journal_size_limit = N`.
    ///
    /// [size_wal_file]: https://www.sqlite.org/wal.html#avoiding_excessively_large_wal_files
    /// [`PRAGMA journal_size_limit`]: https://www.sqlite.org/pragma.html#pragma_journal_size_limit
    async fn journal_size_limit(&self, limit: u32) -> Result<()> {
        self.execute_batch(format!("PRAGMA journal_size_limit = {limit};")).await?;
        Ok(())
    }

    /// Defragment the database and free space on the filesystem.
    ///
    /// Only returns an error in tests, otherwise the error is only logged.
    async fn vacuum(&self) -> Result<()> {
        if let Err(error) = self.execute_batch("VACUUM").await {
            // Since this is an optimisation step, do not propagate the error
            // but log it.
            #[cfg(not(any(test, debug_assertions)))]
            tracing::warn!("Failed to vacuum database: {error}");

            // We want to know if there is an error with this step during tests.
            #[cfg(any(test, debug_assertions))]
            return Err(error.into());
        }

        Ok(())
    }
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
    fn chunk_large_query_over<Key, Query, Res>(
        &self,
        keys_to_chunk: Vec<Key>,
        result_capacity: Option<usize>,
        do_query: Query,
    ) -> Result<Vec<Res>>
    where
        Res: Send + 'static,
        Query: Fn(&Transaction<'_>, Vec<Key>) -> Result<Vec<Res>> + Send + 'static;
}

impl SqliteTransactionExt for Transaction<'_> {
    fn chunk_large_query_over<Key, Query, Res>(
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

    /// Store the given value for the given key by serializing it.
    fn set_serialized_kv<T: Serialize + Send>(&self, key: &str, value: T) -> Result<()> {
        let serialized_value = rmp_serde::to_vec_named(&value)?;
        self.set_kv(key, &serialized_value)?;

        Ok(())
    }

    /// Removes the current key and value if exists.
    fn clear_kv(&self, key: &str) -> rusqlite::Result<()>;

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

    fn clear_kv(&self, key: &str) -> rusqlite::Result<()> {
        self.execute("DELETE FROM kv WHERE key = ?1", (key,))?;
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

    /// Get the stored serialized value for the given key.
    async fn get_serialized_kv<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let Some(bytes) = self.get_kv(key).await? else {
            return Ok(None);
        };

        Ok(Some(rmp_serde::from_slice(&bytes)?))
    }

    /// Store the given value for the given key.
    async fn set_kv(&self, key: &str, value: Vec<u8>) -> rusqlite::Result<()>;

    /// Store the given value for the given key by serializing it.
    async fn set_serialized_kv<T: Serialize + Send + 'static>(
        &self,
        key: &str,
        value: T,
    ) -> Result<()>;

    /// Clears the given value for the given key.
    async fn clear_kv(&self, key: &str) -> rusqlite::Result<()>;

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

    async fn set_serialized_kv<T: Serialize + Send + 'static>(
        &self,
        key: &str,
        value: T,
    ) -> Result<()> {
        let key = key.to_owned();
        self.interact(move |conn| conn.set_serialized_kv(&key, value)).await.unwrap()?;

        Ok(())
    }

    async fn clear_kv(&self, key: &str) -> rusqlite::Result<()> {
        let key = key.to_owned();
        self.interact(move |conn| conn.clear_kv(&key)).await.unwrap()?;

        Ok(())
    }
}

/// Repeat `?` n times, where n is defined by `count`. `?` are comma-separated.
pub(crate) fn repeat_vars(count: usize) -> impl fmt::Display {
    assert_ne!(count, 0, "Can't generate zero repeated vars");

    iter::repeat_n("?", count).format(",")
}

/// Convert the given `SystemTime` to a timestamp, as the number of seconds
/// since Unix Epoch.
///
/// Returns an `i64` as it is the numeric type used by SQLite.
pub(crate) fn time_to_timestamp(time: SystemTime) -> i64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|d| d.as_secs().try_into().ok())
        // It is unlikely to happen unless the time on the system is seriously wrong, but we always
        // need a value.
        .unwrap_or(0)
}

#[cfg(test)]
mod unit_tests {
    use std::time::Duration;

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

    #[test]
    fn test_time_to_timestamp() {
        assert_eq!(time_to_timestamp(SystemTime::UNIX_EPOCH), 0);
        assert_eq!(time_to_timestamp(SystemTime::UNIX_EPOCH + Duration::from_secs(60)), 60);

        // Fallback value on overflow.
        assert_eq!(time_to_timestamp(SystemTime::UNIX_EPOCH - Duration::from_secs(60)), 0);
    }
}
