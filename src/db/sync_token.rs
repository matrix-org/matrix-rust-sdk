//! Database interface for sync tokens

use anyhow::Result;
use sqlx::{
    database::HasArguments, ColumnIndex, Database, Decode, Encode, Executor, IntoArguments, Type,
};

use crate::{StateStore, SupportedDatabase};

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Put a sync token into the sync token store
    ///
    /// # Errors
    /// This function will return an error if the upsert cannot be performed
    async fn save_sync_token(&self, token: &str) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'q> Vec<u8>: Encode<'q, DB>,
        Vec<u8>: Type<DB>,
    {
        self.insert_kv(b"sync_token".to_vec(), token.as_bytes().to_vec())
            .await
    }

    /// Get the last stored sync token
    ///
    /// # Errors
    /// This function will return an error if the database query fails
    pub async fn get_sync_token(&self) -> Result<Option<String>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'q> Vec<u8>: Encode<'q, DB>,
        Vec<u8>: Type<DB>,
        for<'r> Vec<u8>: Decode<'r, DB>,
        for<'a> &'a str: ColumnIndex<<DB as Database>::Row>,
    {
        let result = self.get_kv(b"sync_token".to_vec()).await?;
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
    async fn test_sqlite_sync_token() {
        let store = crate::db::tests::open_sqlite_database().await.unwrap();
        assert_eq!(store.get_sync_token().await.unwrap(), None);
        store.save_sync_token("test").await.unwrap();
        assert_eq!(
            store.get_sync_token().await.unwrap(),
            Some("test".to_owned())
        );
    }

    #[cfg(feature = "mysql")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_mysql_sync_token() {
        let store = crate::db::tests::open_mysql_database().await.unwrap();
        assert_eq!(store.get_sync_token().await.unwrap(), None);
        store.save_sync_token("test").await.unwrap();
        assert_eq!(
            store.get_sync_token().await.unwrap(),
            Some("test".to_owned())
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_sync_token() {
        let store = crate::db::tests::open_postgres_database().await.unwrap();
        assert_eq!(store.get_sync_token().await.unwrap(), None);
        store.save_sync_token("test").await.unwrap();
        assert_eq!(
            store.get_sync_token().await.unwrap(),
            Some("test".to_owned())
        );
    }
}
