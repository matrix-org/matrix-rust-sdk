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
use rusqlite::{Params, Row, Statement, Transaction};

#[derive(Debug)]
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
    async fn execute<P>(&self, sql: &'static str, params: P) -> rusqlite::Result<usize>
    where
        P: Params + Send + 'static;

    async fn execute_batch(&self, sql: &'static str) -> rusqlite::Result<()>;

    async fn prepare<T, F>(&self, sql: &'static str, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Statement<'_>) -> rusqlite::Result<T> + Send + 'static;

    async fn query_row<T, P, F>(&self, sql: &'static str, params: P, f: F) -> rusqlite::Result<T>
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
    async fn execute<P>(&self, sql: &'static str, params: P) -> rusqlite::Result<usize>
    where
        P: Params + Send + 'static,
    {
        self.interact(move |conn| conn.execute(sql, params)).await.unwrap()
    }

    async fn execute_batch(&self, sql: &'static str) -> rusqlite::Result<()> {
        self.interact(move |conn| conn.execute_batch(sql)).await.unwrap()
    }

    async fn prepare<T, F>(&self, sql: &'static str, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        F: FnOnce(Statement<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        self.interact(move |conn| f(conn.prepare(sql)?)).await.unwrap()
    }

    async fn query_row<T, P, F>(&self, sql: &'static str, params: P, f: F) -> rusqlite::Result<T>
    where
        T: Send + 'static,
        P: Params + Send + 'static,
        F: FnOnce(&Row<'_>) -> rusqlite::Result<T> + Send + 'static,
    {
        self.interact(move |conn| conn.query_row(sql, params, f)).await.unwrap()
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
