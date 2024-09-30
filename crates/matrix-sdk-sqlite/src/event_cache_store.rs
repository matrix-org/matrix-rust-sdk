use std::{borrow::Cow, fmt, path::Path, sync::Arc};

use async_trait::async_trait;
use deadpool_sqlite::{Object as SqliteAsyncConn, Pool as SqlitePool, Runtime};
use matrix_sdk_base::{
    event_cache_store::{EventCacheStore, MediaRetentionPolicy},
    media::{MediaRequest, UniqueKey},
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::time::SystemTime;
use rusqlite::{params_from_iter, OptionalExtension};
use tokio::fs;
use tracing::debug;

use crate::{
    error::{Error, Result},
    utils::{
        repeat_vars, time_to_timestamp, Key, SqliteAsyncConnExt, SqliteKeyValueStoreAsyncConnExt,
        SqliteKeyValueStoreConnExt, SqliteTransactionExt,
    },
    OpenStoreError,
};

mod keys {
    // Entries in Key-value store
    pub const MEDIA_RETENTION_POLICY: &str = "media-retention-policy";

    // Tables
    pub const MEDIA: &str = "media";
}

/// Identifier of the latest database version.
///
/// This is used to figure whether the SQLite database requires a migration.
/// Every new SQL migration should imply a bump of this number, and changes in
/// the [`SqliteEventCacheStore::run_migrations`] function.
const DATABASE_VERSION: u8 = 1;

/// A SQLite-based event cache store.
#[derive(Clone)]
pub struct SqliteEventCacheStore {
    store_cipher: Option<Arc<StoreCipher>>,
    pool: SqlitePool,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SqliteEventCacheStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteEventCacheStore").finish_non_exhaustive()
    }
}

impl SqliteEventCacheStore {
    /// Open the SQLite-based event cache store at the given path using the
    /// given passphrase to encrypt private data.
    pub async fn open(
        path: impl AsRef<Path>,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        let pool = create_pool(path.as_ref()).await?;

        Self::open_with_pool(pool, passphrase).await
    }

    /// Open an SQLite-based event cache store using the given SQLite database
    /// pool. The given passphrase will be used to encrypt private data.
    pub async fn open_with_pool(
        pool: SqlitePool,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        let conn = pool.get().await?;
        let version = conn.db_version().await?;
        run_migrations(&conn, version).await?;

        let store_cipher = match passphrase {
            Some(p) => Some(Arc::new(conn.get_or_create_store_cipher(p).await?)),
            None => None,
        };

        Ok(Self { store_cipher, pool })
    }

    fn encode_value(&self, value: Vec<u8>) -> Result<Vec<u8>> {
        if let Some(key) = &self.store_cipher {
            let encrypted = key.encrypt_value_data(value)?;
            Ok(rmp_serde::to_vec_named(&encrypted)?)
        } else {
            Ok(value)
        }
    }

    fn decode_value<'a>(&self, value: &'a [u8]) -> Result<Cow<'a, [u8]>> {
        if let Some(key) = &self.store_cipher {
            let encrypted = rmp_serde::from_slice(value)?;
            let decrypted = key.decrypt_value_data(encrypted)?;
            Ok(Cow::Owned(decrypted))
        } else {
            Ok(Cow::Borrowed(value))
        }
    }

    fn encode_key(&self, table_name: &str, key: impl AsRef<[u8]>) -> Key {
        let bytes = key.as_ref();
        if let Some(store_cipher) = &self.store_cipher {
            Key::Hashed(store_cipher.hash_key(table_name, bytes))
        } else {
            Key::Plain(bytes.to_owned())
        }
    }

    async fn acquire(&self) -> Result<SqliteAsyncConn> {
        Ok(self.pool.get().await?)
    }
}

async fn create_pool(path: &Path) -> Result<SqlitePool, OpenStoreError> {
    fs::create_dir_all(path).await.map_err(OpenStoreError::CreateDir)?;
    let cfg = deadpool_sqlite::Config::new(path.join("matrix-sdk-event-cache.sqlite3"));
    Ok(cfg.create_pool(Runtime::Tokio1)?)
}

/// Run migrations for the given version of the database.
async fn run_migrations(conn: &SqliteAsyncConn, version: u8) -> Result<()> {
    if version == 0 {
        debug!("Creating database");
    } else if version < DATABASE_VERSION {
        debug!(version, new_version = DATABASE_VERSION, "Upgrading database");
    } else {
        return Ok(());
    }

    if version < 1 {
        // First turn on WAL mode, this can't be done in the transaction, it fails with
        // the error message: "cannot change into wal mode from within a transaction".
        conn.execute_batch("PRAGMA journal_mode = wal;").await?;
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/event_cache_store/001_init.sql"))?;
            txn.set_db_version(1)
        })
        .await?;
    }

    Ok(())
}

#[async_trait]
impl EventCacheStore for SqliteEventCacheStore {
    type Error = Error;

    async fn media_retention_policy(&self) -> Result<Option<MediaRetentionPolicy>, Self::Error> {
        let conn = self.acquire().await?;
        let Some(bytes) = conn.get_kv(keys::MEDIA_RETENTION_POLICY).await? else {
            return Ok(None);
        };

        Ok(Some(rmp_serde::from_slice(&bytes)?))
    }

    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        let conn = self.acquire().await?;
        let serialized_policy = rmp_serde::to_vec_named(&policy)?;

        conn.set_kv(keys::MEDIA_RETENTION_POLICY, serialized_policy).await?;
        Ok(())
    }

    async fn add_media_content(
        &self,
        request: &MediaRequest,
        content: Vec<u8>,
        current_time: SystemTime,
        policy: MediaRetentionPolicy,
    ) -> Result<()> {
        let data = self.encode_value(content)?;

        if policy.exceeds_max_file_size(data.len()) {
            // The content is too big to be cached.
            return Ok(());
        }

        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());
        let timestamp = time_to_timestamp(current_time);

        let conn = self.acquire().await?;
        conn.execute(
            "INSERT OR REPLACE INTO media (uri, format, data, last_access) VALUES (?, ?, ?, ?)",
            (uri, format, data, timestamp),
        )
        .await?;

        Ok(())
    }

    async fn get_media_content(
        &self,
        request: &MediaRequest,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>> {
        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());
        let timestamp = time_to_timestamp(current_time);

        let conn = self.acquire().await?;
        let data = conn
            .with_transaction::<_, rusqlite::Error, _>(move |txn| {
                // Update the last access.
                // We need to do this first so the transaction is in write mode right away.
                // See: https://sqlite.org/lang_transaction.html#read_transactions_versus_write_transactions
                txn.execute(
                    "UPDATE media SET last_access = ? WHERE uri = ? AND format = ?",
                    (timestamp, &uri, &format),
                )?;

                txn.query_row::<Vec<u8>, _, _>(
                    "SELECT data FROM media WHERE uri = ? AND format = ?",
                    (&uri, &format),
                    |row| row.get(0),
                )
                .optional()
            })
            .await?;

        data.map(|v| self.decode_value(&v).map(Into::into)).transpose()
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());

        let conn = self.acquire().await?;
        conn.execute("DELETE FROM media WHERE uri = ? AND format = ?", (uri, format)).await?;

        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &ruma::MxcUri) -> Result<()> {
        let uri = self.encode_key(keys::MEDIA, uri);

        let conn = self.acquire().await?;
        conn.execute("DELETE FROM media WHERE uri = ?", (uri,)).await?;

        Ok(())
    }

    async fn clean_up_media_cache(
        &self,
        policy: MediaRetentionPolicy,
        current_time: SystemTime,
    ) -> Result<(), Self::Error> {
        if !policy.has_limitations() {
            // We can safely skip all the checks.
            return Ok(());
        }

        let conn = self.acquire().await?;
        conn.with_transaction::<_, Error, _>(move |txn| {
            // First, check media content that exceed the max filesize.
            if let Some(max_file_size) = policy.computed_max_file_size() {
                txn.execute("DELETE FROM media WHERE length(data) > ?", (max_file_size,))?;
            }

            // Then, clean up expired media content.
            if let Some(last_access_expiry) = policy.last_access_expiry {
                let current_timestamp = time_to_timestamp(current_time);
                let expiry_secs = last_access_expiry.as_secs();
                txn.execute(
                    "DELETE FROM media WHERE (? - last_access) >= ?",
                    (current_timestamp, expiry_secs),
                )?;
            }

            // Finally, if the cache size is too big, remove old items until it fits.
            if let Some(max_cache_size) = policy.max_cache_size {
                // i64 is the integer type used by SQLite, use it here to avoid usize overflow
                // during the conversion of the result.
                let cache_size_int = txn
                    .query_row("SELECT sum(length(data)) FROM media", (), |row| {
                        // `sum()` returns `NULL` if there are no rows.
                        row.get::<_, Option<i64>>(0)
                    })?
                    .unwrap_or_default();
                let cache_size_usize = usize::try_from(cache_size_int);

                // If the cache size is overflowing or bigger than max cache size, clean up.
                if cache_size_usize.is_err()
                    || cache_size_usize.is_ok_and(|cache_size| cache_size > max_cache_size)
                {
                    // Get the sizes of the media contents ordered by last access.
                    let mut cached_stmt = txn.prepare_cached(
                        "SELECT rowid, length(data) FROM media ORDER BY last_access DESC",
                    )?;
                    let content_sizes = cached_stmt
                        .query(())?
                        .mapped(|row| Ok((row.get::<_, i64>(0)?, row.get::<_, usize>(1)?)));

                    let mut accumulated_items_size = 0usize;
                    let mut limit_reached = false;
                    let mut rows_to_remove = Vec::new();

                    for result in content_sizes {
                        let (row_id, size) = match result {
                            Ok(content_size) => content_size,
                            Err(error) => {
                                return Err(error.into());
                            }
                        };

                        if limit_reached {
                            rows_to_remove.push(row_id);
                            continue;
                        }

                        match accumulated_items_size.checked_add(size) {
                            Some(acc) if acc > max_cache_size => {
                                // We can stop accumulating.
                                limit_reached = true;
                                rows_to_remove.push(row_id);
                            }
                            Some(acc) => accumulated_items_size = acc,
                            None => {
                                // The accumulated size is overflowing but the setting cannot be
                                // bigger than usize::MAX, we can stop accumulating.
                                limit_reached = true;
                                rows_to_remove.push(row_id);
                            }
                        };
                    }

                    txn.chunk_large_query_over(rows_to_remove, None, |txn, row_ids| {
                        let sql_params = repeat_vars(row_ids.len());
                        let query = format!("DELETE FROM media WHERE rowid IN ({sql_params})");
                        txn.prepare(&query)?.execute(params_from_iter(row_ids))?;
                        Ok(Vec::<()>::new())
                    })?;
                }
            }

            Ok(())
        })
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU32, Ordering::SeqCst},
        time::Duration,
    };

    use matrix_sdk_base::{
        event_cache_store::{EventCacheStore, EventCacheStoreError, MediaRetentionPolicy},
        event_cache_store_integration_tests,
        media::{MediaFormat, MediaRequest, MediaThumbnailSettings},
    };
    use matrix_sdk_test::async_test;
    use once_cell::sync::Lazy;
    use ruma::{events::room::MediaSource, media::Method, mxc_uri, time::SystemTime, uint};
    use tempfile::{tempdir, TempDir};

    use super::SqliteEventCacheStore;
    use crate::utils::SqliteAsyncConnExt;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());
    static NUM: AtomicU32 = AtomicU32::new(0);

    async fn get_event_cache_store() -> Result<SqliteEventCacheStore, EventCacheStoreError> {
        let name = NUM.fetch_add(1, SeqCst).to_string();
        let tmpdir_path = TMP_DIR.path().join(name);

        tracing::info!("using event cache store @ {}", tmpdir_path.to_str().unwrap());

        Ok(SqliteEventCacheStore::open(tmpdir_path.to_str().unwrap(), None).await.unwrap())
    }

    event_cache_store_integration_tests!(with_media_size_tests);

    async fn get_event_cache_store_content_sorted_by_last_access(
        event_cache_store: &SqliteEventCacheStore,
    ) -> Vec<Vec<u8>> {
        let sqlite_db = event_cache_store.acquire().await.expect("accessing sqlite db failed");
        sqlite_db
            .prepare("SELECT data FROM media ORDER BY last_access DESC", |mut stmt| {
                stmt.query(())?.mapped(|row| row.get(0)).collect()
            })
            .await
            .expect("querying media cache content by last access failed")
    }

    #[async_test]
    async fn test_last_access() {
        let event_cache_store = get_event_cache_store().await.expect("creating media cache failed");
        let uri = mxc_uri!("mxc://localhost/media");
        let file_request =
            MediaRequest { source: MediaSource::Plain(uri.to_owned()), format: MediaFormat::File };
        let thumbnail_request = MediaRequest {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(
                Method::Crop,
                uint!(100),
                uint!(100),
            )),
        };

        let content: Vec<u8> = "hello world".into();
        let thumbnail_content: Vec<u8> = "hello…".into();
        let policy = MediaRetentionPolicy::empty();

        // Add the media.
        let mut time = SystemTime::UNIX_EPOCH;
        event_cache_store
            .add_media_content(&file_request, content.clone(), time, policy)
            .await
            .expect("adding file failed");

        // Add the thumbnail 3 seconds later.
        time = time.checked_add(Duration::from_secs(3)).expect("time should be fine");
        event_cache_store
            .add_media_content(&thumbnail_request, thumbnail_content.clone(), time, policy)
            .await
            .expect("adding thumbnail failed");

        // File's last access is older than thumbnail.
        let contents =
            get_event_cache_store_content_sorted_by_last_access(&event_cache_store).await;

        assert_eq!(contents.len(), 2, "media cache contents length is wrong");
        assert_eq!(contents[0], thumbnail_content, "thumbnail is not last access");
        assert_eq!(contents[1], content, "file is not second-to-last access");

        // Access the file 1 hour later so its last access is more recent.
        time = time.checked_add(Duration::from_secs(3600)).expect("time should be fine");
        let _ = event_cache_store
            .get_media_content(&file_request, time)
            .await
            .expect("getting file failed")
            .expect("file is missing");

        // File's last access is more recent than thumbnail.
        let contents =
            get_event_cache_store_content_sorted_by_last_access(&event_cache_store).await;

        assert_eq!(contents.len(), 2, "media cache contents length is wrong");
        assert_eq!(contents[0], content, "file is not last access");
        assert_eq!(contents[1], thumbnail_content, "thumbnail is not second-to-last access");
    }
}

#[cfg(test)]
mod encrypted_tests {
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    use matrix_sdk_base::{
        event_cache_store::EventCacheStoreError, event_cache_store_integration_tests,
    };
    use once_cell::sync::Lazy;
    use tempfile::{tempdir, TempDir};

    use super::SqliteEventCacheStore;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());
    static NUM: AtomicU32 = AtomicU32::new(0);

    async fn get_event_cache_store() -> Result<SqliteEventCacheStore, EventCacheStoreError> {
        let name = NUM.fetch_add(1, SeqCst).to_string();
        let tmpdir_path = TMP_DIR.path().join(name);

        tracing::info!("using event cache store @ {}", tmpdir_path.to_str().unwrap());

        Ok(SqliteEventCacheStore::open(
            tmpdir_path.to_str().unwrap(),
            Some("default_test_password"),
        )
        .await
        .unwrap())
    }

    event_cache_store_integration_tests!();
}
