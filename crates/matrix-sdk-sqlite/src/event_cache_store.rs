use std::{borrow::Cow, fmt, path::Path, sync::Arc};

use async_trait::async_trait;
use deadpool_sqlite::{Object as SqliteAsyncConn, Pool as SqlitePool, Runtime};
use matrix_sdk_base::{
    event_cache::{store::EventCacheStore, Event, Gap},
    linked_chunk::{ChunkIdentifier, Update},
    media::{MediaRequestParameters, UniqueKey},
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{MilliSecondsSinceUnixEpoch, RoomId};
use rusqlite::{OptionalExtension, Transaction};
use tokio::fs;
use tracing::{debug, trace};

use crate::{
    error::{Error, Result},
    utils::{Key, SqliteAsyncConnExt, SqliteKeyValueStoreAsyncConnExt, SqliteKeyValueStoreConnExt},
    OpenStoreError,
};

mod keys {
    // Tables
    pub const LINKED_CHUNKS: &str = "linked_chunks";
    pub const MEDIA: &str = "media";
}

/// Identifier of the latest database version.
///
/// This is used to figure whether the SQLite database requires a migration.
/// Every new SQL migration should imply a bump of this number, and changes in
/// the [`run_migrations`] function.
const DATABASE_VERSION: u8 = 3;

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

    if version < 2 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/event_cache_store/002_lease_locks.sql"))?;
            txn.set_db_version(2)
        })
        .await?;
    }

    if version < 3 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/event_cache_store/003_events.sql"))?;
            txn.set_db_version(3)
        })
        .await?;
    }

    Ok(())
}

#[async_trait]
impl EventCacheStore for SqliteEventCacheStore {
    type Error = Error;

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool> {
        let key = key.to_owned();
        let holder = holder.to_owned();

        let now: u64 = MilliSecondsSinceUnixEpoch::now().get().into();
        let expiration = now + lease_duration_ms as u64;

        let num_touched = self
            .acquire()
            .await?
            .with_transaction(move |txn| {
                txn.execute(
                    "INSERT INTO lease_locks (key, holder, expiration)
                    VALUES (?1, ?2, ?3)
                    ON CONFLICT (key)
                    DO
                        UPDATE SET holder = ?2, expiration = ?3
                        WHERE holder = ?2
                        OR expiration < ?4
                ",
                    (key, holder, expiration, now),
                )
            })
            .await?;

        Ok(num_touched == 1)
    }

    async fn handle_linked_chunk_updates(
        &self,
        room_id: &RoomId,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error> {
        let hashed_room_id = self.encode_key(keys::LINKED_CHUNKS, room_id);

        for up in updates {
            match up {
                Update::NewItemsChunk { previous, new, next } => {
                    let hashed_room_id = hashed_room_id.clone();

                    let previous = previous.as_ref().map(ChunkIdentifier::index);
                    let new = new.index();
                    let next = next.as_ref().map(ChunkIdentifier::index);

                    trace!(
                        %room_id,
                        "new events chunk (prev={previous:?}, i={new}, next={next:?})",
                    );

                    self.acquire()
                        .await?
                        .with_transaction(move |txn| {
                            insert_chunk(txn, &hashed_room_id, previous, new, next, "E")
                        })
                        .await?;
                }

                Update::NewGapChunk { previous, new, next, gap } => {
                    let hashed_room_id = hashed_room_id.clone();

                    let serialized = serde_json::to_vec(&gap.prev_token)?;
                    let prev_token = self.encode_value(serialized)?;

                    let previous = previous.as_ref().map(ChunkIdentifier::index);
                    let new = new.index();
                    let next = next.as_ref().map(ChunkIdentifier::index);

                    trace!(
                        %room_id,
                        "new gap chunk (prev={previous:?}, i={new}, next={next:?})",
                    );

                    self.acquire()
                        .await?
                        .with_transaction(move |txn| -> rusqlite::Result<()> {
                            // Insert the chunk as a gap.
                            insert_chunk(txn, &hashed_room_id, previous, new, next, "G")?;

                            // Insert the gap's value.
                            // XXX(bnjbvr): might as well inline in the linked_chunks table? better
                            // for flexibility to use another table though.
                            txn.execute(
                                r#"
                                INSERT INTO gaps(chunk_id, room_id, prev_token)
                                VALUES (?, ?, ?)
                            "#,
                                (new, hashed_room_id, prev_token),
                            )?;

                            Ok(())
                        })
                        .await?;
                }

                Update::RemoveChunk(chunk_identifier) => {
                    let hashed_room_id = hashed_room_id.clone();
                    let chunk_id = chunk_identifier.index();

                    trace!(%room_id, "removing chunk @ {chunk_id}");

                    self.acquire()
                        .await?
                        .with_transaction(move |txn| -> rusqlite::Result<()> {
                            // Find chunk to delete.
                            let (previous, next): (Option<usize>, Option<usize>) = txn.query_row(
                                "SELECT previous, next FROM linked_chunks WHERE id = ? AND room_id = ?",
                                (chunk_id, &hashed_room_id),
                                |row| Ok((row.get(0)?, row.get(1)?))
                            )?;

                            // Replace its previous' next to its own next.
                            if let Some(previous) = previous {
                                txn.execute("UPDATE linked_chunks SET next = ? WHERE id = ? AND room_id = ?", (next, previous, &hashed_room_id))?;
                            }

                            // Replace its next' previous to its own previous.
                            if let Some(next) = next {
                                txn.execute("UPDATE linked_chunks SET previous = ? WHERE id = ? AND room_id = ?", (previous, next, &hashed_room_id))?;
                            }

                            // Now delete it, and let cascading delete corresponding entries in the
                            // other data tables.
                            txn.execute("DELETE FROM linked_chunks WHERE id = ? AND room_id = ?", (chunk_id, hashed_room_id))?;

                            Ok(())
                        })
                        .await?;
                }

                Update::PushItems { at, items } => {
                    let chunk_id = at.chunk_identifier().index();
                    let hashed_room_id = hashed_room_id.clone();

                    trace!(%room_id, "pushing items @ {chunk_id}");

                    let this = self.clone();

                    self.acquire()
                        .await?
                        .with_transaction(move |txn| -> Result<(), Self::Error> {
                            for (i, event) in items.into_iter().enumerate() {
                                let serialized = serde_json::to_vec(&event)?;
                                let content = this.encode_value(serialized)?;

                                let event_id =
                                    event.event_id().map(|event_id| event_id.to_string());
                                let index = at.index() + i;

                                txn.execute(
                                    r#"
                                    INSERT INTO events(chunk_id, room_id, event_id, content, position)
                                    VALUES (?, ?, ?, ?, ?)
                                "#,
                                    (chunk_id, &hashed_room_id, event_id, content, index),
                                )?;
                            }

                            Ok(())
                        })
                        .await?;
                }

                Update::RemoveItem { at } => {
                    let hashed_room_id = hashed_room_id.clone();
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "removing item @ {chunk_id}:{index}");

                    self.acquire()
                        .await?
                        .with_transaction(move |txn| -> rusqlite::Result<()> {
                            // Remove the entry.
                            txn.execute("DELETE FROM events WHERE room_id = ? AND chunk_id = ? AND position = ?", (&hashed_room_id, chunk_id, index))?;

                            // Decrement the index of each item after the one we're going to
                            // remove.
                            txn.execute(
                                r#"
                                    UPDATE events
                                    SET position = position - 1
                                    WHERE room_id = ? AND chunk_id = ? AND position > ?
                                "#,
                                (&hashed_room_id, chunk_id, index)
                            )?;

                            Ok(())
                        })
                        .await?;
                }

                Update::DetachLastItems { at } => {
                    let hashed_room_id = hashed_room_id.clone();
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "truncating items >= {chunk_id}:{index}");

                    self.acquire()
                        .await?
                        .with_transaction(move |txn| -> rusqlite::Result<()> {
                            // Remove these entries.
                            txn.execute("DELETE FROM events WHERE room_id = ? AND chunk_id = ? AND position >= ?", (&hashed_room_id, chunk_id, index))?;
                            Ok(())
                        })
                        .await?;
                }

                Update::Clear => {
                    let hashed_room_id = hashed_room_id.clone();

                    trace!(%room_id, "clearing items");

                    self.acquire()
                        .await?
                        .with_transaction(move |txn| {
                            // Remove chunks, and let cascading do its job.
                            txn.execute(
                                "DELETE FROM linked_chunks WHERE room_id = ?",
                                (&hashed_room_id,),
                            )
                        })
                        .await?;
                }

                Update::StartReattachItems | Update::EndReattachItems => {
                    // Nothing.
                }
            }
        }

        Ok(())
    }

    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
    ) -> Result<()> {
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

    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), Self::Error> {
        let prev_uri = self.encode_key(keys::MEDIA, from.source.unique_key());
        let prev_format = self.encode_key(keys::MEDIA, from.format.unique_key());

        let new_uri = self.encode_key(keys::MEDIA, to.source.unique_key());
        let new_format = self.encode_key(keys::MEDIA, to.format.unique_key());

        let conn = self.acquire().await?;
        conn.execute(
            r#"UPDATE media SET uri = ?, format = ?, last_access = CAST(strftime('%s') as INT)
               WHERE uri = ? AND format = ?"#,
            (new_uri, new_format, prev_uri, prev_format),
        )
        .await?;

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequestParameters) -> Result<Option<Vec<u8>>> {
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

    async fn remove_media_content(&self, request: &MediaRequestParameters) -> Result<()> {
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

fn insert_chunk(
    txn: &Transaction<'_>,
    room_id: &Key,
    previous: Option<u64>,
    new: u64,
    next: Option<u64>,
    type_str: &str,
) -> rusqlite::Result<()> {
    // First, insert the new chunk.
    txn.execute(
        r#"
            INSERT INTO linked_chunks(id, room_id, previous, next, type)
            VALUES (?, ?, ?, ?, ?)
        "#,
        (new, room_id, previous, next, type_str),
    )?;

    // If this chunk has a previous one, update its `next` field.
    if let Some(previous) = previous {
        txn.execute(
            r#"
                UPDATE linked_chunks
                SET next = ?
                WHERE id = ? AND room_id = ?
            "#,
            (new, previous, room_id),
        )?;
    }

    // If this chunk has a next one, update its `previous` field.
    if let Some(next) = next {
        txn.execute(
            r#"
                UPDATE linked_chunks
                SET previous = ?
                WHERE id = ? AND room_id = ?
            "#,
            (new, next, room_id),
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU32, Ordering::SeqCst},
        time::Duration,
    };

    use matrix_sdk_base::{
        event_cache::store::{EventCacheStore, EventCacheStoreError},
        event_cache_store_integration_tests, event_cache_store_integration_tests_time,
        media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
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
    event_cache_store_integration_tests_time!();

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
        let file_request = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::File,
        };
        let thumbnail_request = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::Thumbnail(MediaThumbnailSettings::with_method(
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
        event_cache::store::EventCacheStoreError, event_cache_store_integration_tests,
        event_cache_store_integration_tests_time,
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
    event_cache_store_integration_tests_time!();
}
