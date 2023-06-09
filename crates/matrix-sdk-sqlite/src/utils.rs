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

use std::ops::Deref;

use async_trait::async_trait;
use rusqlite::{OptionalExtension, Params, Row, Statement, Transaction};

use crate::OpenStoreError;

pub(crate) fn chain<T>(
    it1: impl IntoIterator<Item = T>,
    it2: impl IntoIterator<Item = T>,
) -> impl Iterator<Item = T> {
    it1.into_iter().chain(it2)
}

#[derive(Clone, Debug)]
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

impl rusqlite::ToSql for Key {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        self.deref().to_sql()
    }
}

#[async_trait]
pub(crate) trait SqliteObjectExt {
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
}

#[async_trait]
impl SqliteObjectExt for deadpool_sqlite::Object {
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
}

pub(crate) trait SqliteConnectionExt {
    fn set_kv(&self, key: &str, value: &[u8]) -> rusqlite::Result<()>;
}

impl SqliteConnectionExt for rusqlite::Connection {
    fn set_kv(&self, key: &str, value: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO kv VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
            (key, value),
        )?;
        Ok(())
    }
}

#[async_trait]
pub(crate) trait SqliteObjectStoreExt: SqliteObjectExt {
    async fn get_kv(&self, key: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        let key = key.to_owned();
        self.query_row("SELECT value FROM kv WHERE key = ?", (key,), |row| row.get(0))
            .await
            .optional()
    }

    async fn set_kv(&self, key: &str, value: Vec<u8>) -> rusqlite::Result<()>;
}

#[async_trait]
impl SqliteObjectStoreExt for deadpool_sqlite::Object {
    async fn set_kv(&self, key: &str, value: Vec<u8>) -> rusqlite::Result<()> {
        let key = key.to_owned();
        self.interact(move |conn| conn.set_kv(&key, &value)).await.unwrap()?;

        Ok(())
    }
}

/// Load the version of the database with the given connection.
pub(crate) async fn load_db_version(conn: &deadpool_sqlite::Object) -> Result<u8, OpenStoreError> {
    let kv_exists = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'kv'",
            (),
            |row| row.get::<_, u32>(0),
        )
        .await
        .map_err(OpenStoreError::LoadVersion)?
        > 0;

    if kv_exists {
        match conn.get_kv("version").await.map_err(OpenStoreError::LoadVersion)?.as_deref() {
            Some([v]) => Ok(*v),
            Some(_) => Err(OpenStoreError::InvalidVersion),
            None => Err(OpenStoreError::MissingVersion),
        }
    } else {
        Ok(0)
    }
}
