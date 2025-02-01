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

use std::{borrow::Cow, fmt, path::Path, sync::Arc};

use async_trait::async_trait;
use deadpool_sqlite::{Object as SqliteAsyncConn, Pool as SqlitePool, Runtime};
use matrix_sdk_base::{
    event_cache::{
        store::{
            media::{
                EventCacheStoreMedia, IgnoreMediaRetentionPolicy, MediaRetentionPolicy,
                MediaService,
            },
            EventCacheStore,
        },
        Event, Gap,
    },
    linked_chunk::{ChunkContent, ChunkIdentifier, RawChunk, Update},
    media::{MediaRequestParameters, UniqueKey},
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{time::SystemTime, MilliSecondsSinceUnixEpoch, MxcUri, RoomId};
use rusqlite::{params_from_iter, OptionalExtension, Transaction, TransactionBehavior};
use tokio::fs;
#[cfg(not(test))]
use tracing::warn;
use tracing::{debug, trace};

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
    pub const MEDIA_RETENTION_POLICY: &str = "media_retention_policy";

    // Tables
    pub const LINKED_CHUNKS: &str = "linked_chunks";
    pub const MEDIA: &str = "media";
}

/// Identifier of the latest database version.
///
/// This is used to figure whether the SQLite database requires a migration.
/// Every new SQL migration should imply a bump of this number, and changes in
/// the [`run_migrations`] function.
const DATABASE_VERSION: u8 = 4;

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
    media_service: Arc<MediaService>,
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
        conn.set_journal_size_limit().await?;

        let version = conn.db_version().await?;
        run_migrations(&conn, version).await?;
        conn.optimize().await?;

        let store_cipher = match passphrase {
            Some(p) => Some(Arc::new(conn.get_or_create_store_cipher(p).await?)),
            None => None,
        };

        let media_service = MediaService::new();
        let media_retention_policy = media_retention_policy(&conn).await?;
        media_service.restore(media_retention_policy);

        Ok(Self { store_cipher, pool, media_service: Arc::new(media_service) })
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
    ) -> Result<RawChunk<Event, Gap>>;

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
    ) -> Result<RawChunk<Event, Gap>> {
        let previous = previous.map(ChunkIdentifier::new);
        let next = next.map(ChunkIdentifier::new);
        let id = ChunkIdentifier::new(id);

        match chunk_type {
            CHUNK_TYPE_GAP_TYPE_STRING => {
                // It's a gap! There's at most one row for it in the database, so a
                // call to `query_row` is sufficient.
                let gap = self.load_gap_content(store, room_id, id)?;
                Ok(RawChunk { content: ChunkContent::Gap(gap), previous, identifier: id, next })
            }

            CHUNK_TYPE_EVENT_TYPE_STRING => {
                // It's events!
                let events = self.load_events_content(store, room_id, id)?;
                Ok(RawChunk {
                    content: ChunkContent::Items(events),
                    previous,
                    identifier: id,
                    next,
                })
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

    if version < 4 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/004_ignore_policy.sql"
            ))?;
            txn.set_db_version(4)
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
        // Use a single transaction throughout this function, so that either all updates
        // work, or none is taken into account.
        let hashed_room_id = self.encode_key(keys::LINKED_CHUNKS, room_id);
        let room_id = room_id.to_owned();
        let this = self.clone();

        with_immediate_transaction(self.acquire().await?, move |txn| {
                for up in updates {
                    match up {
                        Update::NewItemsChunk { previous, new, next } => {
                            let previous = previous.as_ref().map(ChunkIdentifier::index);
                            let new = new.index();
                            let next = next.as_ref().map(ChunkIdentifier::index);

                            trace!(
                                %room_id,
                                "new events chunk (prev={previous:?}, i={new}, next={next:?})",
                            );

                            insert_chunk(
                                txn,
                                &hashed_room_id,
                                previous,
                                new,
                                next,
                                CHUNK_TYPE_EVENT_TYPE_STRING,
                            )?;
                        }

                        Update::NewGapChunk { previous, new, next, gap } => {
                            let serialized = serde_json::to_vec(&gap.prev_token)?;
                            let prev_token = this.encode_value(serialized)?;

                            let previous = previous.as_ref().map(ChunkIdentifier::index);
                            let new = new.index();
                            let next = next.as_ref().map(ChunkIdentifier::index);

                            trace!(
                                %room_id,
                                "new gap chunk (prev={previous:?}, i={new}, next={next:?})",
                            );

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
                                (new, &hashed_room_id, prev_token),
                            )?;
                        }

                        Update::RemoveChunk(chunk_identifier) => {
                            let chunk_id = chunk_identifier.index();

                            trace!(%room_id, "removing chunk @ {chunk_id}");

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
                            txn.execute("DELETE FROM linked_chunks WHERE id = ? AND room_id = ?", (chunk_id, &hashed_room_id))?;
                        }

                        Update::PushItems { at, items } => {
                            let chunk_id = at.chunk_identifier().index();

                            trace!(%room_id, "pushing {} items @ {chunk_id}", items.len());

                            for (i, event) in items.into_iter().enumerate() {
                                let serialized = serde_json::to_vec(&event)?;
                                let content = this.encode_value(serialized)?;

                                let event_id = event.event_id().map(|event_id| event_id.to_string());
                                let index = at.index() + i;

                                txn.execute(
                                    r#"
                                    INSERT INTO events(chunk_id, room_id, event_id, content, position)
                                    VALUES (?, ?, ?, ?, ?)
                                "#,
                                    (chunk_id, &hashed_room_id, event_id, content, index),
                                )?;
                            }
                        }

                        Update::ReplaceItem { at, item: event } => {
                            let chunk_id = at.chunk_identifier().index();
                            let index = at.index();

                            trace!(%room_id, "replacing item @ {chunk_id}:{index}");

                            let serialized = serde_json::to_vec(&event)?;
                            let content = this.encode_value(serialized)?;

                            // The event id should be the same, but just in case it changed…
                            let event_id = event.event_id().map(|event_id| event_id.to_string());

                            txn.execute(
                                r#"
                                UPDATE events
                                SET content = ?, event_id = ?
                                WHERE room_id = ? AND chunk_id = ? AND position = ?
                            "#,
                                (content, event_id, &hashed_room_id, chunk_id, index,)
                            )?;
                        }

                        Update::RemoveItem { at } => {
                            let chunk_id = at.chunk_identifier().index();
                            let index = at.index();

                            trace!(%room_id, "removing item @ {chunk_id}:{index}");

                            // Remove the entry.
                            txn.execute("DELETE FROM events WHERE room_id = ? AND chunk_id = ? AND position = ?", (&hashed_room_id, chunk_id, index))?;

                            // Decrement the index of each item after the one we're going to remove.
                            txn.execute(
                                r#"
                                    UPDATE events
                                    SET position = position - 1
                                    WHERE room_id = ? AND chunk_id = ? AND position > ?
                                "#,
                                (&hashed_room_id, chunk_id, index)
                            )?;

                        }

                        Update::DetachLastItems { at } => {
                            let chunk_id = at.chunk_identifier().index();
                            let index = at.index();

                            trace!(%room_id, "truncating items >= {chunk_id}:{index}");

                            // Remove these entries.
                            txn.execute("DELETE FROM events WHERE room_id = ? AND chunk_id = ? AND position >= ?", (&hashed_room_id, chunk_id, index))?;
                        }

                        Update::Clear => {
                            trace!(%room_id, "clearing items");

                            // Remove chunks, and let cascading do its job.
                            txn.execute(
                                "DELETE FROM linked_chunks WHERE room_id = ?",
                                (&hashed_room_id,),
                            )?;
                        }

                        Update::StartReattachItems | Update::EndReattachItems => {
                            // Nothing.
                        }
                    }
                }

                Ok(())
            })
        .await?;

        Ok(())
    }

    async fn reload_linked_chunk(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<RawChunk<Event, Gap>>, Self::Error> {
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

    async fn clear_all_rooms_chunks(&self) -> Result<(), Self::Error> {
        self.acquire()
            .await?
            .with_transaction(move |txn| {
                // Remove all the chunks, and let cascading do its job.
                txn.execute("DELETE FROM linked_chunks", ())
            })
            .await?;
        Ok(())
    }

    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<()> {
        self.media_service.add_media_content(self, request, content, ignore_policy).await
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
            r#"UPDATE media SET uri = ?, format = ? WHERE uri = ? AND format = ?"#,
            (new_uri, new_format, prev_uri, prev_format),
        )
        .await?;

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequestParameters) -> Result<Option<Vec<u8>>> {
        self.media_service.get_media_content(self, request).await
    }

    async fn remove_media_content(&self, request: &MediaRequestParameters) -> Result<()> {
        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());

        let conn = self.acquire().await?;
        conn.execute("DELETE FROM media WHERE uri = ? AND format = ?", (uri, format)).await?;

        Ok(())
    }

    async fn get_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        self.media_service.get_media_content_for_uri(self, uri).await
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let uri = self.encode_key(keys::MEDIA, uri);

        let conn = self.acquire().await?;
        conn.execute("DELETE FROM media WHERE uri = ?", (uri,)).await?;

        Ok(())
    }

    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        self.media_service.set_media_retention_policy(self, policy).await
    }

    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        self.media_service.media_retention_policy()
    }

    async fn set_ignore_media_retention_policy(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        self.media_service.set_ignore_media_retention_policy(self, request, ignore_policy).await
    }

    async fn clean_up_media_cache(&self) -> Result<(), Self::Error> {
        self.media_service.clean_up_media_cache(self).await
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl EventCacheStoreMedia for SqliteEventCacheStore {
    type Error = Error;

    async fn media_retention_policy_inner(
        &self,
    ) -> Result<Option<MediaRetentionPolicy>, Self::Error> {
        let conn = self.acquire().await?;
        media_retention_policy(&conn).await
    }

    async fn set_media_retention_policy_inner(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        let conn = self.acquire().await?;

        let serialized_policy = rmp_serde::to_vec_named(&policy)?;
        conn.set_kv(keys::MEDIA_RETENTION_POLICY, serialized_policy).await?;

        Ok(())
    }

    async fn add_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        data: Vec<u8>,
        last_access: SystemTime,
        policy: MediaRetentionPolicy,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        let ignore_policy = ignore_policy.is_yes();
        let data = self.encode_value(data)?;

        if !ignore_policy && policy.exceeds_max_file_size(data.len()) {
            return Ok(());
        }

        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());
        let timestamp = time_to_timestamp(last_access);

        let conn = self.acquire().await?;
        conn.execute(
            "INSERT OR REPLACE INTO media (uri, format, data, last_access, ignore_policy) VALUES (?, ?, ?, ?, ?)",
            (uri, format, data, timestamp, ignore_policy),
        )
        .await?;

        Ok(())
    }

    async fn set_ignore_media_retention_policy_inner(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());
        let ignore_policy = ignore_policy.is_yes();

        let conn = self.acquire().await?;
        conn.execute(
            r#"UPDATE media SET ignore_policy = ? WHERE uri = ? AND format = ?"#,
            (ignore_policy, uri, format),
        )
        .await?;

        Ok(())
    }

    async fn get_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
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

    async fn get_media_content_for_uri_inner(
        &self,
        uri: &MxcUri,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        let uri = self.encode_key(keys::MEDIA, uri);
        let timestamp = time_to_timestamp(current_time);

        let conn = self.acquire().await?;
        let data = conn
            .with_transaction::<_, rusqlite::Error, _>(move |txn| {
                // Update the last access.
                // We need to do this first so the transaction is in write mode right away.
                // See: https://sqlite.org/lang_transaction.html#read_transactions_versus_write_transactions
                txn.execute("UPDATE media SET last_access = ? WHERE uri = ?", (timestamp, &uri))?;

                txn.query_row::<Vec<u8>, _, _>(
                    "SELECT data FROM media WHERE uri = ?",
                    (&uri,),
                    |row| row.get(0),
                )
                .optional()
            })
            .await?;

        data.map(|v| self.decode_value(&v).map(Into::into)).transpose()
    }

    async fn clean_up_media_cache_inner(
        &self,
        policy: MediaRetentionPolicy,
        current_time: SystemTime,
    ) -> Result<(), Self::Error> {
        if !policy.has_limitations() {
            // We can safely skip all the checks.
            return Ok(());
        }

        let conn = self.acquire().await?;
        let removed = conn
            .with_transaction::<_, Error, _>(move |txn| {
                let mut removed = false;

                // First, check media content that exceed the max filesize.
                if let Some(max_file_size) = policy.computed_max_file_size() {
                    let count = txn.execute(
                        "DELETE FROM media WHERE ignore_policy IS FALSE AND length(data) > ?",
                        (max_file_size,),
                    )?;

                    if count > 0 {
                        removed = true;
                    }
                }

                // Then, clean up expired media content.
                if let Some(last_access_expiry) = policy.last_access_expiry {
                    let current_timestamp = time_to_timestamp(current_time);
                    let expiry_secs = last_access_expiry.as_secs();
                    let count = txn.execute(
                        "DELETE FROM media WHERE ignore_policy IS FALSE AND (? - last_access) >= ?",
                        (current_timestamp, expiry_secs),
                    )?;

                    if count > 0 {
                        removed = true;
                    }
                }

                // Finally, if the cache size is too big, remove old items until it fits.
                if let Some(max_cache_size) = policy.max_cache_size {
                    // i64 is the integer type used by SQLite, use it here to avoid usize overflow
                    // during the conversion of the result.
                    let cache_size_int = txn
                        .query_row(
                            "SELECT sum(length(data)) FROM media WHERE ignore_policy IS FALSE",
                            (),
                            |row| {
                                // `sum()` returns `NULL` if there are no rows.
                                row.get::<_, Option<i64>>(0)
                            },
                        )?
                        .unwrap_or_default();
                    let cache_size_usize = usize::try_from(cache_size_int);

                    // If the cache size is overflowing or bigger than max cache size, clean up.
                    if cache_size_usize.is_err()
                        || cache_size_usize.is_ok_and(|cache_size| cache_size > max_cache_size)
                    {
                        // Get the sizes of the media contents ordered by last access.
                        let mut cached_stmt = txn.prepare_cached(
                            "SELECT rowid, length(data) FROM media \
                             WHERE ignore_policy IS FALSE ORDER BY last_access DESC",
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

                        if !rows_to_remove.is_empty() {
                            removed = true;
                        }

                        txn.chunk_large_query_over(rows_to_remove, None, |txn, row_ids| {
                            let sql_params = repeat_vars(row_ids.len());
                            let query = format!("DELETE FROM media WHERE rowid IN ({sql_params})");
                            txn.prepare(&query)?.execute(params_from_iter(row_ids))?;
                            Ok(Vec::<()>::new())
                        })?;
                    }
                }

                Ok(removed)
            })
            .await?;

        // If we removed media, use the VACUUM command to defragment the
        // database and free space on the filesystem.
        if removed {
            if let Err(error) = conn.execute("VACUUM", ()).await {
                // Since this is an optimisation step, do not propagate the error
                // but log it.
                #[cfg(not(test))]
                warn!("Failed to vacuum database: {error}");

                // We want to know if there is an error with this step during tests.
                #[cfg(test)]
                return Err(error.into());
            }
        }

        Ok(())
    }
}

/// Like `deadpool::managed::Object::with_transaction`, but starts the
/// transaction in immediate (write) mode from the beginning, precluding errors
/// of the kind SQLITE_BUSY from happening, for transactions that may involve
/// both reads and writes, and start with a write.
async fn with_immediate_transaction<
    T: Send + 'static,
    F: FnOnce(&Transaction<'_>) -> Result<T, Error> + Send + 'static,
>(
    conn: SqliteAsyncConn,
    f: F,
) -> Result<T, Error> {
    conn.interact(move |conn| -> Result<T, Error> {
        // Start the transaction in IMMEDIATE mode since all updates may cause writes,
        // to avoid read transactions upgrading to write mode and causing
        // SQLITE_BUSY errors. See also: https://www.sqlite.org/lang_transaction.html#deferred_immediate_and_exclusive_transactions
        conn.set_transaction_behavior(TransactionBehavior::Immediate);

        let code = || -> Result<T, Error> {
            let txn = conn.transaction()?;
            let res = f(&txn)?;
            txn.commit()?;
            Ok(res)
        };

        let res = code();

        // Reset the transaction behavior to use Deferred, after this transaction has
        // been run, whether it was successful or not.
        conn.set_transaction_behavior(TransactionBehavior::Deferred);

        res
    })
    .await
    // SAFETY: same logic as in [`deadpool::managed::Object::with_transaction`].`
    .unwrap()
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

/// Get the persisted [`MediaRetentionPolicy`] with the given connection.
async fn media_retention_policy(
    conn: &SqliteAsyncConn,
) -> Result<Option<MediaRetentionPolicy>, Error> {
    let Some(bytes) = conn.get_kv(keys::MEDIA_RETENTION_POLICY).await? else {
        return Ok(None);
    };

    Ok(Some(rmp_serde::from_slice(&bytes)?))
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicU32, Ordering::SeqCst},
        time::Duration,
    };

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        event_cache::{
            store::{
                integration_tests::{check_test_event, make_test_event},
                media::IgnoreMediaRetentionPolicy,
                EventCacheStore, EventCacheStoreError,
            },
            Gap,
        },
        event_cache_store_integration_tests, event_cache_store_integration_tests_time,
        event_cache_store_media_integration_tests,
        linked_chunk::{ChunkContent, ChunkIdentifier, Position, Update},
        media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    };
    use matrix_sdk_test::{async_test, DEFAULT_TEST_ROOM_ID};
    use once_cell::sync::Lazy;
    use ruma::{events::room::MediaSource, media::Method, mxc_uri, room_id, uint};
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
    event_cache_store_media_integration_tests!(with_media_size_tests);

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
            .add_media_content(&file_request, content.clone(), IgnoreMediaRetentionPolicy::No)
            .await
            .expect("adding file failed");

        // Since the precision of the timestamp is in seconds, wait so the timestamps
        // differ.
        tokio::time::sleep(Duration::from_secs(3)).await;

        event_cache_store
            .add_media_content(
                &thumbnail_request,
                thumbnail_content.clone(),
                IgnoreMediaRetentionPolicy::No,
            )
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

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

        assert_eq!(chunks.len(), 3);

        {
            // Chunks are ordered from smaller to bigger IDs.
            let c = chunks.remove(0);
            assert_eq!(c.identifier, ChunkIdentifier::new(13));
            assert_eq!(c.previous, Some(ChunkIdentifier::new(42)));
            assert_eq!(c.next, Some(ChunkIdentifier::new(37)));
            assert_matches!(c.content, ChunkContent::Items(events) => {
                assert!(events.is_empty());
            });

            let c = chunks.remove(0);
            assert_eq!(c.identifier, ChunkIdentifier::new(37));
            assert_eq!(c.previous, Some(ChunkIdentifier::new(13)));
            assert_eq!(c.next, None);
            assert_matches!(c.content, ChunkContent::Items(events) => {
                assert!(events.is_empty());
            });

            let c = chunks.remove(0);
            assert_eq!(c.identifier, ChunkIdentifier::new(42));
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

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        // Chunks are ordered from smaller to bigger IDs.
        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Gap(gap) => {
            assert_eq!(gap.prev_token, "raclette");
        });
    }

    #[async_test]
    async fn test_linked_chunk_replace_item() {
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
                    Update::ReplaceItem {
                        at: Position::new(ChunkIdentifier::new(42), 1),
                        item: make_test_event(room_id, "yolo"),
                    },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 2);
            check_test_event(&events[0], "hello");
            check_test_event(&events[1], "yolo");
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

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

        assert_eq!(chunks.len(), 2);

        // Chunks are ordered from smaller to bigger IDs.
        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, Some(ChunkIdentifier::new(44)));
        assert_matches!(c.content, ChunkContent::Gap(gap) => {
            assert_eq!(gap.prev_token, "raclette");
        });

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(44));
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

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 3);

            check_test_event(&events[0], "hello");
            check_test_event(&events[1], "world");
            check_test_event(&events[2], "who?");
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

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_test_event(&events[0], "world");
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

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_test_event(&events[0], "hello");
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

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 3);
            check_test_event(&events[0], "hello");
            check_test_event(&events[1], "world");
            check_test_event(&events[2], "howdy");
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

        let chunks = store.reload_linked_chunk(room_id).await.unwrap();
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
        let mut chunks_room1 = store.reload_linked_chunk(room1).await.unwrap();
        assert_eq!(chunks_room1.len(), 1);

        let c = chunks_room1.remove(0);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 2);
            check_test_event(&events[0], "best cheese is raclette");
            check_test_event(&events[1], "obviously");
        });

        // Check chunks from room 2.
        let mut chunks_room2 = store.reload_linked_chunk(room2).await.unwrap();
        assert_eq!(chunks_room2.len(), 1);

        let c = chunks_room2.remove(0);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_test_event(&events[0], "beaufort is the best");
        });
    }

    #[async_test]
    async fn test_linked_chunk_update_is_a_transaction() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        // Trigger a violation of the unique constraint on the (room id, chunk id)
        // couple.
        let err = store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                ],
            )
            .await
            .unwrap_err();

        // The operation fails with a constraint violation error.
        assert_matches!(err, crate::error::Error::Sqlite(err) => {
            assert_matches!(err.sqlite_error_code(), Some(rusqlite::ErrorCode::ConstraintViolation));
        });

        // If the updates have been handled transactionally, then no new chunks should
        // have been added; failure of the second update leads to the first one being
        // rolled back.
        let chunks = store.reload_linked_chunk(room_id).await.unwrap();
        assert!(chunks.is_empty());
    }
}

#[cfg(test)]
mod encrypted_tests {
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    use matrix_sdk_base::{
        event_cache::store::EventCacheStoreError, event_cache_store_integration_tests,
        event_cache_store_integration_tests_time, event_cache_store_media_integration_tests,
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
    event_cache_store_media_integration_tests!();
}
