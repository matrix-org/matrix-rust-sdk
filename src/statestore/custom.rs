//! Database interface for custom values

use anyhow::Result;
use sqlx::{database::HasArguments, ColumnIndex, Database, Executor, IntoArguments};

use crate::{
    helpers::{BorrowedSqlType, SqlType},
    StateStore, SupportedDatabase,
};

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Put arbitrary data into the custom store
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub async fn set_custom_value(&self, key_ref: &[u8], val: &[u8]) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
    {
        let mut key = Vec::with_capacity(7 + key_ref.len());
        key.extend_from_slice(b"custom:");
        key.extend_from_slice(key_ref);

        self.insert_kv(&key, val).await
    }

    /// Get arbitrary data from the custom store
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub async fn get_custom_value(&self, key_ref: &[u8]) -> Result<Option<Vec<u8>>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
        Vec<u8>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let mut key = Vec::with_capacity(7 + key_ref.len());
        key.extend_from_slice(b"custom:");
        key.extend_from_slice(key_ref);
        self.get_kv(&key).await
    }
}

#[cfg(test)]
#[allow(unused_imports, unreachable_pub, clippy::unwrap_used)]
mod tests {
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_custom_values() {
        let store = crate::statestore::tests::open_sqlite_database()
            .await
            .unwrap();
        assert_eq!(store.get_custom_value(b"test").await.unwrap(), None);
        store.set_custom_value(b"test", b"test").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test".to_vec())
        );
        store.set_custom_value(b"test2", b"test3").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test2").await.unwrap(),
            Some(b"test3".to_vec())
        );
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test".to_vec())
        );
        store.set_custom_value(b"test", b"test4").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test4".to_vec())
        );
        assert_eq!(
            store.get_custom_value(b"test2").await.unwrap(),
            Some(b"test3".to_vec())
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_custom_values() {
        let store = crate::statestore::tests::open_postgres_database()
            .await
            .unwrap();
        assert_eq!(store.get_custom_value(b"test").await.unwrap(), None);
        store.set_custom_value(b"test", b"test").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test".to_vec())
        );
        store.set_custom_value(b"test2", b"test3").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test2").await.unwrap(),
            Some(b"test3".to_vec())
        );
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test".to_vec())
        );
        store.set_custom_value(b"test", b"test4").await.unwrap();
        assert_eq!(
            store.get_custom_value(b"test").await.unwrap(),
            Some(b"test4".to_vec())
        );
        assert_eq!(
            store.get_custom_value(b"test2").await.unwrap(),
            Some(b"test3".to_vec())
        );
    }
}
