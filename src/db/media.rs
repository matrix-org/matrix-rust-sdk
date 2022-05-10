//! Media database interface

use anyhow::Result;
use matrix_sdk_base::media::MediaRequest;
use ruma::{events::room::MediaSource, MxcUri};
use sqlx::{
    database::HasArguments, ColumnIndex, Decode, Encode, Executor, IntoArguments, Row, Transaction,
    Type,
};

use crate::{StateStore, SupportedDatabase};

impl<DB: SupportedDatabase> StateStore<DB> {
    /// Insert media into the media store
    ///
    /// # Errors
    /// This function will return an error if the media cannot be inserted
    pub async fn insert_media(&self, url: &MxcUri, media: Vec<u8>) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'a, 'c> &'c mut Transaction<'a, DB>: Executor<'c, Database = DB>,
        for<'q> Vec<u8>: Encode<'q, DB>,
        for<'q> String: Encode<'q, DB>,
        Vec<u8>: Type<DB>,
        String: Type<DB>,
    {
        let mut txn = self.db.begin().await?;

        DB::media_insert_query_1()
            .bind(url.to_string())
            .bind(media)
            .execute(&mut txn)
            .await?;
        DB::media_insert_query_2().execute(&mut txn).await?;

        txn.commit().await?;
        Ok(())
    }

    /// Deletes media from the media store
    ///
    /// # Errors
    /// This function will return an error if the media cannot be deleted
    pub async fn delete_media(&self, url: &MxcUri) -> Result<()>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'q> String: Encode<'q, DB>,
        String: Type<DB>,
    {
        DB::media_delete_query()
            .bind(url.to_string())
            .execute(&*self.db)
            .await?;
        Ok(())
    }

    /// Gets media from the media store
    ///
    /// # Errors
    /// This function will return an error if the query fails
    pub async fn get_media(&self, url: &MxcUri) -> Result<Option<Vec<u8>>>
    where
        for<'a> <DB as HasArguments<'a>>::Arguments: IntoArguments<'a, DB>,
        for<'c> &'c mut <DB as sqlx::Database>::Connection: Executor<'c, Database = DB>,
        for<'q> String: Encode<'q, DB>,
        String: Type<DB>,
        Vec<u8>: Type<DB>,
        for<'r> Vec<u8>: Decode<'r, DB>,
        for<'a> &'a str: ColumnIndex<<DB as sqlx::Database>::Row>,
    {
        let row = DB::media_load_query()
            .bind(url.to_string())
            .fetch_optional(&*self.db)
            .await?;
        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };
        Ok(row.try_get("media_data")?)
    }

    /// Extracts an [`MxcUri`] from a media query
    ///
    /// [`MxcUri`]: ruma::identifiers::MxcUri
    #[must_use]
    pub fn extract_media_url(request: &MediaRequest) -> &MxcUri {
        match request.source {
            MediaSource::Plain(ref p) => p,
            MediaSource::Encrypted(ref e) => &e.url,
        }
    }
}

#[cfg(test)]
#[allow(unused_imports, unreachable_pub, clippy::unwrap_used)]
mod tests {
    use ruma::{MxcUri, OwnedMxcUri};
    #[cfg(feature = "sqlite")]
    #[tokio::test]
    async fn test_sqlite_mediastore() {
        let store = crate::db::tests::open_sqlite_database().await.unwrap();
        let entry_0 = <&MxcUri>::from("mxc://localhost:8080/media/0");
        let entry_1 = <&MxcUri>::from("mxc://localhost:8080/media/1");

        store
            .insert_media(entry_0, b"media_0".to_vec())
            .await
            .unwrap();
        store
            .insert_media(entry_1, b"media_1".to_vec())
            .await
            .unwrap();

        for entry in 2..101 {
            let entry = OwnedMxcUri::from(format!("mxc://localhost:8080/media/{}", entry));
            store
                .insert_media(&entry, b"media_0".to_vec())
                .await
                .unwrap();
        }

        assert_eq!(store.get_media(entry_0).await.unwrap(), None);
        assert_eq!(
            store.get_media(entry_1).await.unwrap(),
            Some(b"media_1".to_vec())
        );
    }

    #[cfg(feature = "postgres")]
    #[tokio::test]
    #[cfg_attr(not(feature = "ci"), ignore)]
    async fn test_postgres_mediastore() {
        let store = crate::db::tests::open_postgres_database().await.unwrap();
        let entry_0 = <&MxcUri>::from("mxc://localhost:8080/media/0");
        let entry_1 = <&MxcUri>::from("mxc://localhost:8080/media/1");

        store
            .insert_media(entry_0, b"media_0".to_vec())
            .await
            .unwrap();
        store
            .insert_media(entry_1, b"media_1".to_vec())
            .await
            .unwrap();

        for entry in 2..101 {
            let entry = OwnedMxcUri::from(format!("mxc://localhost:8080/media/{}", entry));
            store
                .insert_media(&entry, b"media_0".to_vec())
                .await
                .unwrap();
        }

        assert_eq!(store.get_media(entry_0).await.unwrap(), None);
        assert_eq!(
            store.get_media(entry_1).await.unwrap(),
            Some(b"media_1".to_vec())
        );
    }
}
