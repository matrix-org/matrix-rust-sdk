// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! A sqlite-based backend for the [`EventCacheStore`].

#![allow(dead_code)] // Most of the unused code may be used soonish.

use std::{borrow::Cow, fmt, path::Path, sync::Arc};

use async_trait::async_trait;
use deadpool_sqlite::{Object as SqliteAsyncConn, Pool as SqlitePool, Runtime};
use matrix_sdk_base::{
    event_cache::{store::EventCacheStore, Event, Gap},
    linked_chunk::{ChunkContent, ChunkIdentifier, Update},
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

/// The string used to identify a chunk of type events, in the `type` field in
/// the database.
const CHUNK_TYPE_EVENT_TYPE_STRING: &str = "E";
/// The string used to identify a chunk of type gap, in the `type` field in the
/// database.
const CHUNK_TYPE_GAP_TYPE_STRING: &str = "G";

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

    fn map_row_to_chunk(
        row: &rusqlite::Row<'_>,
    ) -> Result<(u64, Option<u64>, Option<u64>, String), rusqlite::Error> {
        Ok((
            row.get::<_, u64>(0)?,
            row.get::<_, Option<u64>>(1)?,
            row.get::<_, Option<u64>>(2)?,
            row.get::<_, String>(3)?,
        ))
    }

    async fn load_chunks(&self, room_id: &RoomId) -> Result<Vec<RawLinkedChunk>> {
        let room_id = room_id.to_owned();
        let hashed_room_id = self.encode_key(keys::LINKED_CHUNKS, &room_id);

        let this = self.clone();

        let result = self
            .acquire()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                let mut items = Vec::new();

                // Use `ORDER BY id` to get a deterministic ordering for testing purposes.
                for data in txn
                    .prepare(
                        "SELECT id, previous, next, type FROM linked_chunks WHERE room_id = ? ORDER BY id",
                    )?
                    .query_map((&hashed_room_id,), Self::map_row_to_chunk)?
                {
                    let (id, previous, next, chunk_type) = data?;
                    let new = txn.rebuild_chunk(
                        &this,
                        &hashed_room_id,
                        previous,
                        id,
                        next,
                        chunk_type.as_str(),
                    )?;
                    items.push(new);
                }

                Ok(items)
            })
            .await?;

        Ok(result)
    }

    async fn load_chunk_with_id(
        &self,
        room_id: &RoomId,
        chunk_id: ChunkIdentifier,
    ) -> Result<RawLinkedChunk> {
        let hashed_room_id = self.encode_key(keys::LINKED_CHUNKS, room_id);

        let this = self.clone();

        self
            .acquire()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                let (id, previous, next, chunk_type) = txn.query_row(
                    "SELECT id, previous, next, type FROM linked_chunks WHERE room_id = ? AND chunk_id = ?", 
                    (&hashed_room_id, chunk_id.index()),
                    Self::map_row_to_chunk
                )?;
                txn.rebuild_chunk(&this, &hashed_room_id, previous, id, next, chunk_type.as_str())
            }).await
    }
}

trait TransactionExtForLinkedChunks {
    fn rebuild_chunk(
        &self,
        store: &SqliteEventCacheStore,
        room_id: &Key,
        previous: Option<u64>,
        index: u64,
        next: Option<u64>,
        chunk_type: &str,
    ) -> Result<RawLinkedChunk>;

    fn load_gap_content(
        &self,
        store: &SqliteEventCacheStore,
        room_id: &Key,
        chunk_id: ChunkIdentifier,
    ) -> Result<Gap>;

    fn load_events_content(
        &self,
        store: &SqliteEventCacheStore,
        room_id: &Key,
        chunk_id: ChunkIdentifier,
    ) -> Result<Vec<Event>>;
}

impl TransactionExtForLinkedChunks for Transaction<'_> {
    fn rebuild_chunk(
        &self,
        store: &SqliteEventCacheStore,
        room_id: &Key,
        previous: Option<u64>,
        id: u64,
        next: Option<u64>,
        chunk_type: &str,
    ) -> Result<RawLinkedChunk> {
        let previous = previous.map(ChunkIdentifier::new);
        let next = next.map(ChunkIdentifier::new);
        let id = ChunkIdentifier::new(id);

        match chunk_type {
            CHUNK_TYPE_GAP_TYPE_STRING => {
                // It's a gap! There's at most one row for it in the database, so a
                // call to `query_row` is sufficient.
                let gap = self.load_gap_content(store, room_id, id)?;
                Ok(RawLinkedChunk { content: ChunkContent::Gap(gap), previous, id, next })
            }

            CHUNK_TYPE_EVENT_TYPE_STRING => {
                // It's events!
                let events = self.load_events_content(store, room_id, id)?;
                Ok(RawLinkedChunk { content: ChunkContent::Items(events), previous, id, next })
            }

            other => {
                // It's an error!
                Err(Error::InvalidData {
                    details: format!("a linked chunk has an unknown type {other}"),
                })
            }
        }
    }

    fn load_gap_content(
        &self,
        store: &SqliteEventCacheStore,
        room_id: &Key,
        chunk_id: ChunkIdentifier,
    ) -> Result<Gap> {
        let encoded_prev_token: Vec<u8> = self.query_row(
            "SELECT prev_token FROM gaps WHERE chunk_id = ? AND room_id = ?",
            (chunk_id.index(), &room_id),
            |row| row.get(0),
        )?;
        let prev_token_bytes = store.decode_value(&encoded_prev_token)?;
        let prev_token = serde_json::from_slice(&prev_token_bytes)?;
        Ok(Gap { prev_token })
    }

    fn load_events_content(
        &self,
        store: &SqliteEventCacheStore,
        room_id: &Key,
        chunk_id: ChunkIdentifier,
    ) -> Result<Vec<Event>> {
        // Retrieve all the events from the database.
        let mut events = Vec::new();

        for event_data in self
            .prepare(
                r#"
                    SELECT content FROM events
                    WHERE chunk_id = ? AND room_id = ?
                    ORDER BY position ASC
                "#,
            )?
            .query_map((chunk_id.index(), &room_id), |row| row.get::<_, Vec<u8>>(0))?
        {
            let encoded_content = event_data?;
            let serialized_content = store.decode_value(&encoded_content)?;
            let sync_timeline_event = serde_json::from_slice(&serialized_content)?;

            events.push(sync_timeline_event);
        }

        Ok(events)
    }
}

struct RawLinkedChunk {
    content: ChunkContent<Event, Gap>,

    previous: Option<ChunkIdentifier>,
    id: ChunkIdentifier,
    next: Option<ChunkIdentifier>,
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
        // Enable foreign keys for this database.
        conn.execute_batch("PRAGMA foreign_keys = ON;").await?;

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
                            insert_chunk(
                                txn,
                                &hashed_room_id,
                                previous,
                                new,
                                next,
                                CHUNK_TYPE_EVENT_TYPE_STRING,
                            )
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
                            insert_chunk(
                                txn,
                                &hashed_room_id,
                                previous,
                                new,
                                next,
                                CHUNK_TYPE_GAP_TYPE_STRING,
                            )?;

                            // Insert the gap's value.
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

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        deserialized_responses::{
            AlgorithmInfo, DecryptedRoomEvent, EncryptionInfo, SyncTimelineEvent,
            TimelineEventKind, VerificationState,
        },
        event_cache::{
            store::{EventCacheStore, EventCacheStoreError},
            Gap,
        },
        event_cache_store_integration_tests, event_cache_store_integration_tests_time,
        linked_chunk::{ChunkContent, ChunkIdentifier, Position, Update},
        media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory, ALICE, DEFAULT_TEST_ROOM_ID};
    use once_cell::sync::Lazy;
    use ruma::{
        events::room::MediaSource, media::Method, mxc_uri, push::Action, room_id, uint, RoomId,
    };
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

    #[async_test]
    async fn test_linked_chunk_new_items_chunk() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None, // Note: the store must link the next entry itself.
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(42)),
                        new: ChunkIdentifier::new(13),
                        next: Some(ChunkIdentifier::new(37)), /* But it's fine to explicitly pass
                                                               * the next link ahead of time. */
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(13)),
                        new: ChunkIdentifier::new(37),
                        next: None,
                    },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.load_chunks(room_id).await.unwrap();

        assert_eq!(chunks.len(), 3);

        {
            // Chunks are ordered from smaller to bigger IDs.
            let c = chunks.remove(0);
            assert_eq!(c.id, ChunkIdentifier::new(13));
            assert_eq!(c.previous, Some(ChunkIdentifier::new(42)));
            assert_eq!(c.next, Some(ChunkIdentifier::new(37)));
            assert_matches!(c.content, ChunkContent::Items(events) => {
                assert!(events.is_empty());
            });

            let c = chunks.remove(0);
            assert_eq!(c.id, ChunkIdentifier::new(37));
            assert_eq!(c.previous, Some(ChunkIdentifier::new(13)));
            assert_eq!(c.next, None);
            assert_matches!(c.content, ChunkContent::Items(events) => {
                assert!(events.is_empty());
            });

            let c = chunks.remove(0);
            assert_eq!(c.id, ChunkIdentifier::new(42));
            assert_eq!(c.previous, None);
            assert_eq!(c.next, Some(ChunkIdentifier::new(13)));
            assert_matches!(c.content, ChunkContent::Items(events) => {
                assert!(events.is_empty());
            });
        }
    }

    #[async_test]
    async fn test_linked_chunk_new_gap_chunk() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![Update::NewGapChunk {
                    previous: None,
                    new: ChunkIdentifier::new(42),
                    next: None,
                    gap: Gap { prev_token: "raclette".to_owned() },
                }],
            )
            .await
            .unwrap();

        let mut chunks = store.load_chunks(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        // Chunks are ordered from smaller to bigger IDs.
        let c = chunks.remove(0);
        assert_eq!(c.id, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Gap(gap) => {
            assert_eq!(gap.prev_token, "raclette");
        });
    }

    #[async_test]
    async fn test_linked_chunk_remove_chunk() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewGapChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                        gap: Gap { prev_token: "raclette".to_owned() },
                    },
                    Update::NewGapChunk {
                        previous: Some(ChunkIdentifier::new(42)),
                        new: ChunkIdentifier::new(43),
                        next: None,
                        gap: Gap { prev_token: "fondue".to_owned() },
                    },
                    Update::NewGapChunk {
                        previous: Some(ChunkIdentifier::new(43)),
                        new: ChunkIdentifier::new(44),
                        next: None,
                        gap: Gap { prev_token: "tartiflette".to_owned() },
                    },
                    Update::RemoveChunk(ChunkIdentifier::new(43)),
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.load_chunks(room_id).await.unwrap();

        assert_eq!(chunks.len(), 2);

        // Chunks are ordered from smaller to bigger IDs.
        let c = chunks.remove(0);
        assert_eq!(c.id, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, Some(ChunkIdentifier::new(44)));
        assert_matches!(c.content, ChunkContent::Gap(gap) => {
            assert_eq!(gap.prev_token, "raclette");
        });

        let c = chunks.remove(0);
        assert_eq!(c.id, ChunkIdentifier::new(44));
        assert_eq!(c.previous, Some(ChunkIdentifier::new(42)));
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Gap(gap) => {
            assert_eq!(gap.prev_token, "tartiflette");
        });

        // Check that cascading worked. Yes, sqlite, I doubt you.
        let gaps = store
            .acquire()
            .await
            .unwrap()
            .with_transaction(|txn| -> rusqlite::Result<_> {
                let mut gaps = Vec::new();
                for data in txn
                    .prepare("SELECT chunk_id FROM gaps ORDER BY chunk_id")?
                    .query_map((), |row| row.get::<_, u64>(0))?
                {
                    gaps.push(data?);
                }
                Ok(gaps)
            })
            .await
            .unwrap();

        assert_eq!(gaps, vec![42, 44]);
    }

    fn make_test_event(room_id: &RoomId, content: &str) -> SyncTimelineEvent {
        let encryption_info = EncryptionInfo {
            sender: (*ALICE).into(),
            sender_device: None,
            algorithm_info: AlgorithmInfo::MegolmV1AesSha2 {
                curve25519_key: "1337".to_owned(),
                sender_claimed_keys: Default::default(),
            },
            verification_state: VerificationState::Verified,
        };

        let event = EventFactory::new()
            .text_msg(content)
            .room(room_id)
            .sender(*ALICE)
            .into_raw_timeline()
            .cast();

        SyncTimelineEvent {
            kind: TimelineEventKind::Decrypted(DecryptedRoomEvent {
                event,
                encryption_info,
                unsigned_encryption_info: None,
            }),
            push_actions: vec![Action::Notify],
        }
    }

    #[track_caller]
    fn check_event(event: &SyncTimelineEvent, text: &str) {
        // Check push actions.
        let actions = &event.push_actions;
        assert_eq!(actions.len(), 1);
        assert_matches!(&actions[0], Action::Notify);

        // Check content.
        assert_matches!(&event.kind, TimelineEventKind::Decrypted(d) => {
            // Check encryption fields.
            assert_eq!(d.encryption_info.sender, *ALICE);
            assert_matches!(&d.encryption_info.algorithm_info, AlgorithmInfo::MegolmV1AesSha2 { curve25519_key, .. } => {
                assert_eq!(curve25519_key, "1337");
            });

            // Check event.
            let deserialized = d.event.deserialize().unwrap();
            assert_matches!(deserialized, ruma::events::AnyMessageLikeEvent::RoomMessage(msg) => {
                assert_eq!(msg.as_original().unwrap().content.body(), text);
            });
        });
    }

    #[async_test]
    async fn test_linked_chunk_push_items() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "hello"),
                            make_test_event(room_id, "world"),
                        ],
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 2),
                        items: vec![make_test_event(room_id, "who?")],
                    },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.load_chunks(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.id, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 3);

            check_event(&events[0], "hello");
            check_event(&events[1], "world");
            check_event(&events[2], "who?");
        });
    }

    #[async_test]
    async fn test_linked_chunk_remove_item() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "hello"),
                            make_test_event(room_id, "world"),
                        ],
                    },
                    Update::RemoveItem { at: Position::new(ChunkIdentifier::new(42), 0) },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.load_chunks(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.id, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_event(&events[0], "world");
        });

        // Make sure the position has been updated for the remaining event.
        let num_rows: u64 = store
            .acquire()
            .await
            .unwrap()
            .with_transaction(move |txn| {
                txn.query_row(
                    "SELECT COUNT(*) FROM events WHERE chunk_id = 42 AND room_id = ? AND position = 0",
                    (room_id.as_bytes(),),
                    |row| row.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(num_rows, 1);
    }

    #[async_test]
    async fn test_linked_chunk_detach_last_items() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "hello"),
                            make_test_event(room_id, "world"),
                            make_test_event(room_id, "howdy"),
                        ],
                    },
                    Update::DetachLastItems { at: Position::new(ChunkIdentifier::new(42), 1) },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.load_chunks(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.id, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_event(&events[0], "hello");
        });
    }

    #[async_test]
    async fn test_linked_chunk_start_end_reattach_items() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        // Same updates and checks as test_linked_chunk_push_items, but with extra
        // `StartReattachItems` and `EndReattachItems` updates, which must have no
        // effects.
        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "hello"),
                            make_test_event(room_id, "world"),
                            make_test_event(room_id, "howdy"),
                        ],
                    },
                    Update::StartReattachItems,
                    Update::EndReattachItems,
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.load_chunks(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.id, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 3);
            check_event(&events[0], "hello");
            check_event(&events[1], "world");
            check_event(&events[2], "howdy");
        });
    }

    #[async_test]
    async fn test_linked_chunk_clear() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::NewGapChunk {
                        previous: Some(ChunkIdentifier::new(42)),
                        new: ChunkIdentifier::new(54),
                        next: None,
                        gap: Gap { prev_token: "fondue".to_owned() },
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "hello"),
                            make_test_event(room_id, "world"),
                            make_test_event(room_id, "howdy"),
                        ],
                    },
                    Update::Clear,
                ],
            )
            .await
            .unwrap();

        let chunks = store.load_chunks(room_id).await.unwrap();
        assert!(chunks.is_empty());
    }

    #[async_test]
    async fn test_linked_chunk_multiple_rooms() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room1 = room_id!("!realcheeselovers:raclette.fr");
        let room2 = room_id!("!realcheeselovers:fondue.ch");

        // Check that applying updates to one room doesn't affect the others.
        // Use the same chunk identifier in both rooms to battle-test search.

        store
            .handle_linked_chunk_updates(
                room1,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room1, "best cheese is raclette"),
                            make_test_event(room1, "obviously"),
                        ],
                    },
                ],
            )
            .await
            .unwrap();

        store
            .handle_linked_chunk_updates(
                room2,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![make_test_event(room1, "beaufort is the best")],
                    },
                ],
            )
            .await
            .unwrap();

        // Check chunks from room 1.
        let mut chunks_room1 = store.load_chunks(room1).await.unwrap();
        assert_eq!(chunks_room1.len(), 1);

        let c = chunks_room1.remove(0);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 2);
            check_event(&events[0], "best cheese is raclette");
            check_event(&events[1], "obviously");
        });

        // Check chunks from room 2.
        let mut chunks_room2 = store.load_chunks(room2).await.unwrap();
        assert_eq!(chunks_room2.len(), 1);

        let c = chunks_room2.remove(0);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_event(&events[0], "beaufort is the best");
        });
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
