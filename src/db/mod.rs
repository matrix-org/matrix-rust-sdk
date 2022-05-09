//! Database code for matrix-sdk-statestore-sql

use crate::{StateStore, SupportedDatabase};
use anyhow::Result;
use sqlx::{
    database::HasArguments, ColumnIndex, Database, Decode, Encode, Executor, IntoArguments, Row,
    Type,
};

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Insert a key-value pair into the kv table
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub async fn insert_kv(&self, key: Vec<u8>, value: Vec<u8>) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'q> Vec<u8>: Encode<'q, DB>,
        Vec<u8>: Type<DB>,
    {
        DB::kv_upsert_query()
            .bind(key)
            .bind(value)
            .execute(&*self.db)
            .await?;
        Ok(())
    }

    /// Get a value from the kv table
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub async fn get_kv(&self, key: Vec<u8>) -> Result<Option<Vec<u8>>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'q> Vec<u8>: Encode<'q, DB>,
        Vec<u8>: Type<DB>,
        for<'r> Vec<u8>: Decode<'r, DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let row = DB::kv_load_query()
            .bind(key)
            .fetch_optional(&*self.db)
            .await?;

        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };

        Ok(row.try_get("kv_value")?)
    }
}

#[cfg(test)]
#[allow(unused_imports, unreachable_pub, clippy::unwrap_used)]
pub mod tests {
    use crate::{StateStore, SupportedDatabase};
    use anyhow::Result;
    use sqlx::{
        database::HasArguments, migrate::Migrate, ColumnIndex, Database, Decode, Encode, Executor,
        IntoArguments, Pool, Type,
    };
    use std::sync::Arc;
    #[cfg(feature = "sqlite")]
    async fn open_sqlite_database() -> Result<StateStore<sqlx::Sqlite>> {
        let db = Arc::new(sqlx::SqlitePool::connect("sqlite://:memory:").await?);
        let store = StateStore::new(&db).await?;
        Ok(store)
    }

    #[cfg(feature = "mysql")]
    async fn open_mysql_database() -> Result<StateStore<sqlx::MySql>> {
        let db =
            Arc::new(sqlx::MySqlPool::connect("mysql://mysql:mysql@localhost:3306/mysql").await?);
        let store = StateStore::new(&db).await?;
        Ok(store)
    }

    #[cfg(feature = "postgres")]
    async fn open_postgres_database() -> Result<StateStore<sqlx::Postgres>> {
        let db = Arc::new(
            sqlx::PgPool::connect("postgres://postgres:postgres@localhost:5432/postgres").await?,
        );
        let store = StateStore::new(&db).await?;
        Ok(store)
    }

    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_kv_store() {
        let store = open_sqlite_database().await.unwrap();
        store
            .insert_kv(b"key".to_vec(), b"value".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value".to_vec()));
        store
            .insert_kv(b"key".to_vec(), b"value2".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value2".to_vec()));
    }

    #[cfg(feature = "mysql")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_mysql_kv_store() {
        let store = open_mysql_database().await.unwrap();
        store
            .insert_kv(b"key".to_vec(), b"value".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value".to_vec()));
        store
            .insert_kv(b"key".to_vec(), b"value2".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value2".to_vec()));
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_kv_store() {
        let store = open_postgres_database().await.unwrap();
        store
            .insert_kv(b"key".to_vec(), b"value".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value".to_vec()));
        store
            .insert_kv(b"key".to_vec(), b"value2".to_vec())
            .await
            .unwrap();
        let value = store.get_kv(b"key".to_vec()).await.unwrap();
        assert_eq!(value, Some(b"value2".to_vec()));
    }
}
