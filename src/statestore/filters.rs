//! Database interface for filters

use anyhow::Result;
use sqlx::{database::HasArguments, ColumnIndex, Database, Executor, IntoArguments};

use crate::{
    helpers::{BorrowedSqlType, SqlType},
    StateStore, SupportedDatabase,
};

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Save the given filter id under the given name
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    pub async fn save_filter(&self, name: &str, filter_id: &str) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
    {
        let mut key = Vec::with_capacity(7 + name.len());
        key.extend_from_slice(b"filter:");
        key.extend_from_slice(name.as_bytes());

        self.insert_kv(&key, filter_id.as_bytes()).await
    }

    /// Get the filter id that was stored under the given filter name.
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub async fn get_filter(&self, name: &str) -> Result<Option<String>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'a> &'a [u8]: BorrowedSqlType<'a, DB>,
        Vec<u8>: SqlType<DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let mut key = Vec::with_capacity(7 + name.len());
        key.extend_from_slice(b"filter:");
        key.extend_from_slice(name.as_bytes());
        let result = self.get_kv(&key).await?;
        match result {
            Some(value) => Ok(Some(String::from_utf8(value)?)),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
#[allow(unused_imports, unreachable_pub, clippy::unwrap_used)]
mod tests {
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_filters() {
        let store = crate::statestore::tests::open_sqlite_database()
            .await
            .unwrap();
        assert_eq!(store.get_filter("test").await.unwrap(), None);
        store.save_filter("test", "test").await.unwrap();
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test".to_owned())
        );
        store.save_filter("test2", "test3").await.unwrap();
        assert_eq!(
            store.get_filter("test2").await.unwrap(),
            Some("test3".to_owned())
        );
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test".to_owned())
        );
        store.save_filter("test", "test4").await.unwrap();
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test4".to_owned())
        );
        assert_eq!(
            store.get_filter("test2").await.unwrap(),
            Some("test3".to_owned())
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_filters() {
        let store = crate::statestore::tests::open_postgres_database()
            .await
            .unwrap();
        assert_eq!(store.get_filter("test").await.unwrap(), None);
        store.save_filter("test", "test").await.unwrap();
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test".to_owned())
        );
        store.save_filter("test2", "test3").await.unwrap();
        assert_eq!(
            store.get_filter("test2").await.unwrap(),
            Some("test3".to_owned())
        );
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test".to_owned())
        );
        store.save_filter("test", "test4").await.unwrap();
        assert_eq!(
            store.get_filter("test").await.unwrap(),
            Some("test4".to_owned())
        );
        assert_eq!(
            store.get_filter("test2").await.unwrap(),
            Some("test3".to_owned())
        );
    }
}
