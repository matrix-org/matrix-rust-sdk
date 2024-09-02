use std::{borrow::Cow, fmt, path::Path, sync::Arc};

use async_trait::async_trait;
use deadpool_sqlite::{Object as SqliteAsyncConn, Pool as SqlitePool, Runtime};
use matrix_sdk_base::{
    event_cache_store::EventCacheStore,
    media::{MediaRequest, UniqueKey},
};
use matrix_sdk_store_encryption::StoreCipher;
use rusqlite::OptionalExtension;
use tokio::fs;
use tracing::debug;

use crate::{
    error::{Error, Result},
    utils::{Key, SqliteAsyncConnExt, SqliteKeyValueStoreAsyncConnExt, SqliteKeyValueStoreConnExt},
    OpenStoreError,
};

mod keys {
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

    async fn add_media_content(&self, request: &MediaRequest, content: Vec<u8>) -> Result<()> {
        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());
        let data = self.encode_value(content)?;

        let conn = self.acquire().await?;
        conn.execute(
            "INSERT OR REPLACE INTO media (uri, format, data, last_access) VALUES (?, ?, ?, CAST(strftime('%s') as INT))",
            (uri, format, data),
        )
        .await?;

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());

        let conn = self.acquire().await?;
        let data = conn
            .with_transaction::<_, rusqlite::Error, _>(move |txn| {
                // Update the last access.
                // We need to do this first so the transaction is in write mode right away.
                // See: https://sqlite.org/lang_transaction.html#read_transactions_versus_write_transactions
                txn.execute(
                    "UPDATE media SET last_access = CAST(strftime('%s') as INT) \
                     WHERE uri = ? AND format = ?",
                    (&uri, &format),
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
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU32, Ordering::SeqCst},
        time::Duration,
    };

    use matrix_sdk_base::{
        event_cache_store::{EventCacheStore, EventCacheStoreError},
        event_cache_store_integration_tests,
        media::{MediaFormat, MediaRequest, MediaThumbnailSettings},
    };
    use matrix_sdk_test::async_test;
    use once_cell::sync::Lazy;
    use ruma::{events::room::MediaSource, media::Method, mxc_uri, uint};
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

    event_cache_store_integration_tests!();

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

        // Add the media.
        event_cache_store
            .add_media_content(&file_request, content.clone())
            .await
            .expect("adding file failed");

        // Since the precision of the timestamp is in seconds, wait so the timestamps
        // differ.
        tokio::time::sleep(Duration::from_secs(3)).await;

        event_cache_store
            .add_media_content(&thumbnail_request, thumbnail_content.clone())
            .await
            .expect("adding thumbnail failed");

        // File's last access is older than thumbnail.
        let contents =
            get_event_cache_store_content_sorted_by_last_access(&event_cache_store).await;

        assert_eq!(contents.len(), 2, "media cache contents length is wrong");
        assert_eq!(contents[0], thumbnail_content, "thumbnail is not last access");
        assert_eq!(contents[1], content, "file is not second-to-last access");

        // Since the precision of the timestamp is in seconds, wait so the timestamps
        // differ.
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Access the file so its last access is more recent.
        let _ = event_cache_store
            .get_media_content(&file_request)
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
