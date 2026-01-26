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

//! An SQLite-based backend for the [`EventCacheStore`].

use std::{collections::HashMap, fmt, iter::once, path::Path, sync::Arc};

use async_trait::async_trait;
use matrix_sdk_base::{
    cross_process_lock::CrossProcessLockGeneration,
    deserialized_responses::TimelineEvent,
    event_cache::{
        Event, Gap,
        store::{EventCacheStore, extract_event_relation},
    },
    linked_chunk::{
        ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId,
        Position, RawChunk, Update,
    },
    timer,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, events::relation::RelationType,
};
use rusqlite::{
    OptionalExtension, ToSql, Transaction, TransactionBehavior, params, params_from_iter,
};
use tokio::{
    fs,
    sync::{Mutex, OwnedMutexGuard},
};
use tracing::{debug, error, instrument, trace};

use crate::{
    OpenStoreError, Secret, SqliteStoreConfig,
    connection::{Connection as SqliteAsyncConn, Pool as SqlitePool},
    error::{Error, Result},
    utils::{
        EncryptableStore, Key, SqliteAsyncConnExt, SqliteKeyValueStoreAsyncConnExt,
        SqliteKeyValueStoreConnExt, SqliteTransactionExt, repeat_vars,
    },
};

mod keys {
    // Tables
    pub const LINKED_CHUNKS: &str = "linked_chunks";
    pub const EVENTS: &str = "events";
}

/// The database name.
const DATABASE_NAME: &str = "matrix-sdk-event-cache.sqlite3";

/// Identifier of the latest database version.
///
/// This is used to figure whether the SQLite database requires a migration.
/// Every new SQL migration should imply a bump of this number, and changes in
/// the [`run_migrations`] function.
const DATABASE_VERSION: u8 = 14;

/// The string used to identify a chunk of type events, in the `type` field in
/// the database.
const CHUNK_TYPE_EVENT_TYPE_STRING: &str = "E";
/// The string used to identify a chunk of type gap, in the `type` field in the
/// database.
const CHUNK_TYPE_GAP_TYPE_STRING: &str = "G";

/// An SQLite-based event cache store.
#[derive(Clone)]
pub struct SqliteEventCacheStore {
    store_cipher: Option<Arc<StoreCipher>>,

    /// The pool of connections.
    pool: SqlitePool,

    /// We make the difference between connections for read operations, and for
    /// write operations. We keep a single connection apart from write
    /// operations. All other connections are used for read operations. The
    /// lock is used to ensure there is one owner at a time.
    write_connection: Arc<Mutex<SqliteAsyncConn>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SqliteEventCacheStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SqliteEventCacheStore").finish_non_exhaustive()
    }
}

impl EncryptableStore for SqliteEventCacheStore {
    fn get_cypher(&self) -> Option<&StoreCipher> {
        self.store_cipher.as_deref()
    }
}

impl SqliteEventCacheStore {
    /// Open the SQLite-based event cache store at the given path using the
    /// given passphrase to encrypt private data.
    pub async fn open(
        path: impl AsRef<Path>,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        Self::open_with_config(SqliteStoreConfig::new(path).passphrase(passphrase)).await
    }

    /// Open the SQLite-based event cache store at the given path using the
    /// given key to encrypt private data.
    pub async fn open_with_key(
        path: impl AsRef<Path>,
        key: Option<&[u8; 32]>,
    ) -> Result<Self, OpenStoreError> {
        Self::open_with_config(SqliteStoreConfig::new(path).key(key)).await
    }

    /// Open the SQLite-based event cache store with the config open config.
    #[instrument(skip(config), fields(path = ?config.path))]
    pub async fn open_with_config(config: SqliteStoreConfig) -> Result<Self, OpenStoreError> {
        debug!(?config);

        let _timer = timer!("open_with_config");

        fs::create_dir_all(&config.path).await.map_err(OpenStoreError::CreateDir)?;

        let pool = config.build_pool_of_connections(DATABASE_NAME)?;

        let this = Self::open_with_pool(pool, config.secret).await?;
        this.write().await?.apply_runtime_config(config.runtime_config).await?;

        Ok(this)
    }

    /// Open an SQLite-based event cache store using the given SQLite database
    /// pool. The given secret will be used to encrypt private data.
    async fn open_with_pool(
        pool: SqlitePool,
        secret: Option<Secret>,
    ) -> Result<Self, OpenStoreError> {
        let conn = pool.get().await?;

        let version = conn.db_version().await?;

        run_migrations(&conn, version).await?;

        conn.wal_checkpoint().await;

        let store_cipher = match secret {
            Some(s) => Some(Arc::new(conn.get_or_create_store_cipher(s).await?)),
            None => None,
        };

        Ok(Self {
            store_cipher,
            pool,
            // Use `conn` as our selected write connections.
            write_connection: Arc::new(Mutex::new(conn)),
        })
    }

    /// Acquire a connection for executing read operations.
    #[instrument(skip_all)]
    async fn read(&self) -> Result<SqliteAsyncConn> {
        let connection = self.pool.get().await?;

        // Per https://www.sqlite.org/foreignkeys.html#fk_enable, foreign key
        // support must be enabled on a per-connection basis. Execute it every
        // time we try to get a connection, since we can't guarantee a previous
        // connection did enable it before.
        connection.execute_batch("PRAGMA foreign_keys = ON;").await?;

        Ok(connection)
    }

    /// Acquire a connection for executing write operations.
    #[instrument(skip_all)]
    async fn write(&self) -> Result<OwnedMutexGuard<SqliteAsyncConn>> {
        let connection = self.write_connection.clone().lock_owned().await;

        // Per https://www.sqlite.org/foreignkeys.html#fk_enable, foreign key
        // support must be enabled on a per-connection basis. Execute it every
        // time we try to get a connection, since we can't guarantee a previous
        // connection did enable it before.
        connection.execute_batch("PRAGMA foreign_keys = ON;").await?;

        Ok(connection)
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

    fn encode_event(&self, event: &TimelineEvent) -> Result<EncodedEvent> {
        let serialized = serde_json::to_vec(event)?;

        // Extract the relationship info here.
        let raw_event = event.raw();
        let (relates_to, rel_type) = extract_event_relation(raw_event).unzip();

        // The content may be encrypted.
        let content = self.encode_value(serialized)?;

        Ok(EncodedEvent {
            content,
            rel_type,
            relates_to: relates_to.map(|relates_to| relates_to.to_string()),
        })
    }

    pub async fn vacuum(&self) -> Result<()> {
        self.write_connection.lock().await.vacuum().await
    }

    async fn get_db_size(&self) -> Result<Option<usize>> {
        Ok(Some(self.pool.get().await?.get_db_size().await?))
    }
}

struct EncodedEvent {
    content: Vec<u8>,
    rel_type: Option<String>,
    relates_to: Option<String>,
}

trait TransactionExtForLinkedChunks {
    fn rebuild_chunk(
        &self,
        store: &SqliteEventCacheStore,
        linked_chunk_id: &Key,
        previous: Option<u64>,
        index: u64,
        next: Option<u64>,
        chunk_type: &str,
    ) -> Result<RawChunk<Event, Gap>>;

    fn load_gap_content(
        &self,
        store: &SqliteEventCacheStore,
        linked_chunk_id: &Key,
        chunk_id: ChunkIdentifier,
    ) -> Result<Gap>;

    fn load_events_content(
        &self,
        store: &SqliteEventCacheStore,
        linked_chunk_id: &Key,
        chunk_id: ChunkIdentifier,
    ) -> Result<Vec<Event>>;
}

impl TransactionExtForLinkedChunks for Transaction<'_> {
    fn rebuild_chunk(
        &self,
        store: &SqliteEventCacheStore,
        linked_chunk_id: &Key,
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
                // It's a gap!
                let gap = self.load_gap_content(store, linked_chunk_id, id)?;
                Ok(RawChunk { content: ChunkContent::Gap(gap), previous, identifier: id, next })
            }

            CHUNK_TYPE_EVENT_TYPE_STRING => {
                // It's events!
                let events = self.load_events_content(store, linked_chunk_id, id)?;
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
        linked_chunk_id: &Key,
        chunk_id: ChunkIdentifier,
    ) -> Result<Gap> {
        // There's at most one row for it in the database, so a call to `query_row` is
        // sufficient.
        let encoded_prev_token: Vec<u8> = self.query_row(
            "SELECT prev_token FROM gap_chunks WHERE chunk_id = ? AND linked_chunk_id = ?",
            (chunk_id.index(), &linked_chunk_id),
            |row| row.get(0),
        )?;
        let prev_token_bytes = store.decode_value(&encoded_prev_token)?;
        let prev_token = serde_json::from_slice(&prev_token_bytes)?;
        Ok(Gap { prev_token })
    }

    fn load_events_content(
        &self,
        store: &SqliteEventCacheStore,
        linked_chunk_id: &Key,
        chunk_id: ChunkIdentifier,
    ) -> Result<Vec<Event>> {
        // Retrieve all the events from the database.
        let mut events = Vec::new();

        for event_data in self
            .prepare(
                r#"
                    SELECT events.content
                    FROM event_chunks ec, events
                    WHERE events.event_id = ec.event_id AND ec.chunk_id = ? AND ec.linked_chunk_id = ?
                    ORDER BY ec.position ASC
                "#,
            )?
            .query_map((chunk_id.index(), &linked_chunk_id), |row| row.get::<_, Vec<u8>>(0))?
        {
            let encoded_content = event_data?;
            let serialized_content = store.decode_value(&encoded_content)?;
            let event = serde_json::from_slice(&serialized_content)?;

            events.push(event);
        }

        Ok(events)
    }
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

    // Always enable foreign keys for the current connection.
    conn.execute_batch("PRAGMA foreign_keys = ON;").await?;

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

    if version < 4 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/004_ignore_policy.sql"
            ))?;
            txn.set_db_version(4)
        })
        .await?;
    }

    if version < 5 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/005_events_index_on_event_id.sql"
            ))?;
            txn.set_db_version(5)
        })
        .await?;
    }

    if version < 6 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/event_cache_store/006_events.sql"))?;
            txn.set_db_version(6)
        })
        .await?;
    }

    if version < 7 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/007_event_chunks.sql"
            ))?;
            txn.set_db_version(7)
        })
        .await?;
    }

    if version < 8 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/008_linked_chunk_id.sql"
            ))?;
            txn.set_db_version(8)
        })
        .await?;
    }

    if version < 9 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/009_related_event_index.sql"
            ))?;
            txn.set_db_version(9)
        })
        .await?;
    }

    if version < 10 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/event_cache_store/010_drop_media.sql"))?;
            txn.set_db_version(10)
        })
        .await?;

        if version >= 1 {
            // Defragment the DB and optimize its size on the filesystem now that we removed
            // the media cache.
            conn.vacuum().await?;
        }
    }

    if version < 11 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/011_empty_event_cache.sql"
            ))?;
            txn.set_db_version(11)
        })
        .await?;
    }

    if version < 12 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/012_store_event_type.sql"
            ))?;
            txn.set_db_version(12)
        })
        .await?;
    }

    if version < 13 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/013_lease_locks_with_generation.sql"
            ))?;
            txn.set_db_version(13)
        })
        .await?;
    }

    if version < 14 {
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!(
                "../migrations/event_cache_store/014_event_chunks_event_id_index.sql"
            ))?;
            txn.set_db_version(14)
        })
        .await?;
    }

    Ok(())
}

#[async_trait]
impl EventCacheStore for SqliteEventCacheStore {
    type Error = Error;

    #[instrument(skip(self))]
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>> {
        let key = key.to_owned();
        let holder = holder.to_owned();

        let now: u64 = MilliSecondsSinceUnixEpoch::now().get().into();
        let expiration = now + lease_duration_ms as u64;

        // Learn about the `excluded` keyword in https://sqlite.org/lang_upsert.html.
        let generation = self
            .write()
            .await?
            .with_transaction(move |txn| {
                txn.query_row(
                    "INSERT INTO lease_locks (key, holder, expiration)
                    VALUES (?1, ?2, ?3)
                    ON CONFLICT (key)
                    DO
                        UPDATE SET
                            holder = excluded.holder,
                            expiration = excluded.expiration,
                            generation =
                                CASE holder
                                    WHEN excluded.holder THEN generation
                                    ELSE generation + 1
                                END
                        WHERE
                            holder = excluded.holder
                            OR expiration < ?4
                    RETURNING generation
                    ",
                    (key, holder, expiration, now),
                    |row| row.get(0),
                )
                .optional()
            })
            .await?;

        Ok(generation)
    }

    #[instrument(skip(self, updates))]
    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error> {
        let _timer = timer!("method");

        // Use a single transaction throughout this function, so that either all updates
        // work, or none is taken into account.
        let hashed_linked_chunk_id =
            self.encode_key(keys::LINKED_CHUNKS, linked_chunk_id.storage_key());
        let linked_chunk_id = linked_chunk_id.to_owned();
        let this = self.clone();

        with_immediate_transaction(self, move |txn| {
            for up in updates {
                match up {
                    Update::NewItemsChunk { previous, new, next } => {
                        let previous = previous.as_ref().map(ChunkIdentifier::index);
                        let new = new.index();
                        let next = next.as_ref().map(ChunkIdentifier::index);

                        trace!(
                            %linked_chunk_id,
                            "new events chunk (prev={previous:?}, i={new}, next={next:?})",
                        );

                        insert_chunk(
                            txn,
                            &hashed_linked_chunk_id,
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
                            %linked_chunk_id,
                            "new gap chunk (prev={previous:?}, i={new}, next={next:?})",
                        );

                        // Insert the chunk as a gap.
                        insert_chunk(
                            txn,
                            &hashed_linked_chunk_id,
                            previous,
                            new,
                            next,
                            CHUNK_TYPE_GAP_TYPE_STRING,
                        )?;

                        // Insert the gap's value.
                        txn.execute(
                            r#"
                            INSERT INTO gap_chunks(chunk_id, linked_chunk_id, prev_token)
                            VALUES (?, ?, ?)
                        "#,
                            (new, &hashed_linked_chunk_id, prev_token),
                        )?;
                    }

                    Update::RemoveChunk(chunk_identifier) => {
                        let chunk_id = chunk_identifier.index();

                        trace!(%linked_chunk_id, "removing chunk @ {chunk_id}");

                        // Find chunk to delete.
                        let (previous, next): (Option<usize>, Option<usize>) = txn.query_row(
                            "SELECT previous, next FROM linked_chunks WHERE id = ? AND linked_chunk_id = ?",
                            (chunk_id, &hashed_linked_chunk_id),
                            |row| Ok((row.get(0)?, row.get(1)?))
                        )?;

                        // Replace its previous' next to its own next.
                        if let Some(previous) = previous {
                            txn.execute("UPDATE linked_chunks SET next = ? WHERE id = ? AND linked_chunk_id = ?", (next, previous, &hashed_linked_chunk_id))?;
                        }

                        // Replace its next' previous to its own previous.
                        if let Some(next) = next {
                            txn.execute("UPDATE linked_chunks SET previous = ? WHERE id = ? AND linked_chunk_id = ?", (previous, next, &hashed_linked_chunk_id))?;
                        }

                        // Now delete it, and let cascading delete corresponding entries in the
                        // other data tables.
                        txn.execute("DELETE FROM linked_chunks WHERE id = ? AND linked_chunk_id = ?", (chunk_id, &hashed_linked_chunk_id))?;
                    }

                    Update::PushItems { at, items } => {
                        if items.is_empty() {
                            // Should never happens, but better be safe.
                            continue;
                        }

                        let chunk_id = at.chunk_identifier().index();

                        trace!(%linked_chunk_id, "pushing {} items @ {chunk_id}", items.len());

                        let mut chunk_statement = txn.prepare(
                            "INSERT INTO event_chunks(chunk_id, linked_chunk_id, event_id, position) VALUES (?, ?, ?, ?)"
                        )?;

                        // Note: we use `OR REPLACE` here, because the event might have been
                        // already inserted in the database. This is the case when an event is
                        // deduplicated and moved to another position; or because it was inserted
                        // outside the context of a linked chunk (e.g. pinned event).
                        let mut content_statement = txn.prepare(
                            "INSERT OR REPLACE INTO events(room_id, event_id, event_type, session_id, content, relates_to, rel_type) VALUES (?, ?, ?, ?, ?, ?, ?)"
                        )?;

                        let invalid_event = |event: TimelineEvent| {
                            let Some(event_id) = event.event_id() else {
                                error!(%linked_chunk_id, "Trying to push an event with no ID");
                                return None;
                            };

                            let Some(event_type) = event.kind.event_type() else {
                                error!(%event_id, "Trying to save an event with no event type");
                                return None;
                            };

                            Some((event_id.to_string(), event_type, event))
                        };

                        let room_id = linked_chunk_id.room_id();
                        let hashed_room_id = this.encode_key(keys::LINKED_CHUNKS, room_id);

                        for (i, (event_id, event_type, event)) in items.into_iter().filter_map(invalid_event).enumerate() {
                            // Insert the location information into the database.
                            let index = at.index() + i;
                            chunk_statement.execute((chunk_id, &hashed_linked_chunk_id, &event_id, index))?;

                            let session_id = event.kind.session_id().map(|s| this.encode_key(keys::EVENTS, s));
                            let event_type = this.encode_key(keys::EVENTS, event_type);

                            // Now, insert the event content into the database.
                            let encoded_event = this.encode_event(&event)?;
                            content_statement.execute((&hashed_room_id, event_id, event_type, session_id, encoded_event.content, encoded_event.relates_to, encoded_event.rel_type))?;
                        }
                    }

                    Update::ReplaceItem { at, item: event } => {
                        let chunk_id = at.chunk_identifier().index();

                        let index = at.index();

                        trace!(%linked_chunk_id, "replacing item @ {chunk_id}:{index}");

                        // The event id should be the same, but just in case it changed…
                        let Some(event_id) = event.event_id().map(|event_id| event_id.to_string()) else {
                            error!(%linked_chunk_id, "Trying to replace an event with a new one that has no ID");
                            continue;
                        };

                        let Some(event_type) = event.kind.event_type() else {
                            error!(%event_id, "Trying to save an event with no event type");
                            continue;
                        };

                        let session_id = event.kind.session_id().map(|s| this.encode_key(keys::EVENTS, s));
                        let event_type = this.encode_key(keys::EVENTS, event_type);

                        // Replace the event's content. Really we'd like to update, but in case the
                        // event id changed, we are a bit lenient here and will allow an insertion
                        // of the new event.
                        let encoded_event = this.encode_event(&event)?;
                        let room_id = linked_chunk_id.room_id();
                        let hashed_room_id = this.encode_key(keys::LINKED_CHUNKS, room_id);
                        txn.execute(
                            "INSERT OR REPLACE INTO events(room_id, event_id, event_type, session_id, content, relates_to, rel_type) VALUES (?, ?, ?, ?, ?, ?, ?)",
                            (&hashed_room_id, &event_id, event_type, session_id, encoded_event.content, encoded_event.relates_to, encoded_event.rel_type))?;

                        // Replace the event id in the linked chunk, in case it changed.
                        txn.execute(
                            r#"UPDATE event_chunks SET event_id = ? WHERE linked_chunk_id = ? AND chunk_id = ? AND position = ?"#,
                            (event_id, &hashed_linked_chunk_id, chunk_id, index)
                        )?;
                    }

                    Update::RemoveItem { at } => {
                        let chunk_id = at.chunk_identifier().index();
                        let index = at.index();

                        trace!(%linked_chunk_id, "removing item @ {chunk_id}:{index}");

                        // Remove the entry in the chunk table.
                        txn.execute("DELETE FROM event_chunks WHERE linked_chunk_id = ? AND chunk_id = ? AND position = ?", (&hashed_linked_chunk_id, chunk_id, index))?;

                        // Decrement the index of each item after the one we are
                        // going to remove.
                        //
                        // Imagine we have the following events:
                        //
                        // | event_id | linked_chunk_id | chunk_id | position |
                        // |----------|-----------------|----------|----------|
                        // | $ev0     | !r0             | 42       | 0        |
                        // | $ev1     | !r0             | 42       | 1        |
                        // | $ev2     | !r0             | 42       | 2        |
                        // | $ev3     | !r0             | 42       | 3        |
                        // | $ev4     | !r0             | 42       | 4        |
                        //
                        // `$ev2` has been removed, then we end up in this
                        // state:
                        //
                        // | event_id | linked_chunk_id    | chunk_id | position |
                        // |----------|--------------------|----------|----------|
                        // | $ev0     | !r0                | 42       | 0        |
                        // | $ev1     | !r0                | 42       | 1        |
                        // |          |                    |          |          | <- no more `$ev2`
                        // | $ev3     | !r0                | 42       | 3        |
                        // | $ev4     | !r0                | 42       | 4        |
                        //
                        // We need to shift the `position` of `$ev3` and `$ev4`
                        // to `position - 1`, like so:
                        //
                        // | event_id | linked_chunk_id | chunk_id | position |
                        // |----------|-----------------|----------|----------|
                        // | $ev0     | !r0             | 42       | 0        |
                        // | $ev1     | !r0             | 42       | 1        |
                        // | $ev3     | !r0             | 42       | 2        |
                        // | $ev4     | !r0             | 42       | 3        |
                        //
                        // Usually, it boils down to run the following query:
                        //
                        // ```sql
                        // UPDATE event_chunks
                        // SET position = position - 1
                        // WHERE position > 2 AND …
                        // ```
                        //
                        // Okay. But `UPDATE` runs on rows in no particular
                        // order. It means that it can update `$ev4` before
                        // `$ev3` for example. What happens in this particular
                        // case? The `position` of `$ev4` becomes `3`, however
                        // `$ev3` already has `position = 3`. Because there
                        // is a `UNIQUE` constraint on `(linked_chunk_id, chunk_id,
                        // position)`, it will result in a constraint violation.
                        //
                        // There is **no way** to control the execution order of
                        // `UPDATE` in SQLite. To persuade yourself, try:
                        //
                        // ```sql
                        // UPDATE event_chunks
                        // SET position = position - 1
                        // FROM (
                        //     SELECT event_id
                        //     FROM event_chunks
                        //     WHERE position > 2 AND …
                        //     ORDER BY position ASC
                        // ) as ordered
                        // WHERE event_chunks.event_id = ordered.event_id
                        // ```
                        //
                        // It will fail the same way.
                        //
                        // Thus, we have 2 solutions:
                        //
                        // 1. Remove the `UNIQUE` constraint,
                        // 2. Be creative.
                        //
                        // The `UNIQUE` constraint is a safe belt. Normally, we
                        // have `event_cache::Deduplicator` that is responsible
                        // to ensure there is no duplicated event. However,
                        // relying on this is “fragile” in the sense it can
                        // contain bugs. Relying on the `UNIQUE` constraint from
                        // SQLite is more robust. It's “braces and belt” as we
                        // say here.
                        //
                        // So. We need to be creative.
                        //
                        // Many solutions exist. Amongst the most popular, we
                        // see _dropping and re-creating the index_, which is
                        // no-go for us, it's too expensive. I (@hywan) have
                        // adopted the following one:
                        //
                        // - Do `position = position - 1` but in the negative
                        //   space, so `position = -(position - 1)`. A position
                        //   cannot be negative; we are sure it is unique!
                        // - Once all candidate rows are updated, do `position =
                        //   -position` to move back to the positive space.
                        //
                        // 'told you it's gonna be creative.
                        //
                        // This solution is a hack, **but** it is a small
                        // number of operations, and we can keep the `UNIQUE`
                        // constraint in place.
                        txn.execute(
                            r#"
                                UPDATE event_chunks
                                SET position = -(position - 1)
                                WHERE linked_chunk_id = ? AND chunk_id = ? AND position > ?
                            "#,
                            (&hashed_linked_chunk_id, chunk_id, index)
                        )?;
                        txn.execute(
                            r#"
                                UPDATE event_chunks
                                SET position = -position
                                WHERE position < 0 AND linked_chunk_id = ? AND chunk_id = ?
                            "#,
                            (&hashed_linked_chunk_id, chunk_id)
                        )?;
                    }

                    Update::DetachLastItems { at } => {
                        let chunk_id = at.chunk_identifier().index();
                        let index = at.index();

                        trace!(%linked_chunk_id, "truncating items >= {chunk_id}:{index}");

                        // Remove these entries.
                        txn.execute("DELETE FROM event_chunks WHERE linked_chunk_id = ? AND chunk_id = ? AND position >= ?", (&hashed_linked_chunk_id, chunk_id, index))?;
                    }

                    Update::Clear => {
                        trace!(%linked_chunk_id, "clearing items");

                        // Remove chunks, and let cascading do its job.
                        txn.execute(
                            "DELETE FROM linked_chunks WHERE linked_chunk_id = ?",
                            (&hashed_linked_chunk_id,),
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

    #[instrument(skip(self))]
    async fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Event, Gap>>, Self::Error> {
        let _timer = timer!("method");

        let hashed_linked_chunk_id =
            self.encode_key(keys::LINKED_CHUNKS, linked_chunk_id.storage_key());

        let this = self.clone();

        let result = self
            .read()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                let mut items = Vec::new();

                // Use `ORDER BY id` to get a deterministic ordering for testing purposes.
                for data in txn
                    .prepare(
                        "SELECT id, previous, next, type FROM linked_chunks WHERE linked_chunk_id = ? ORDER BY id",
                    )?
                    .query_map((&hashed_linked_chunk_id,), Self::map_row_to_chunk)?
                {
                    let (id, previous, next, chunk_type) = data?;
                    let new = txn.rebuild_chunk(
                        &this,
                        &hashed_linked_chunk_id,
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

    #[instrument(skip(self))]
    async fn load_all_chunks_metadata(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<ChunkMetadata>, Self::Error> {
        let _timer = timer!("method");

        let hashed_linked_chunk_id =
            self.encode_key(keys::LINKED_CHUNKS, linked_chunk_id.storage_key());

        self.read()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                // We want to collect the metadata about each chunk (id, next, previous), and
                // for event chunks, the number of events in it. For gaps, the
                // number of events is 0, by convention.
                //
                // We've tried different strategies over time:
                // - use a `LEFT JOIN` + `COUNT`, which was extremely inefficient because it
                //   caused a full table traversal for each chunk, including for gaps which
                //   don't have any events. This happened in
                //   https://github.com/matrix-org/matrix-rust-sdk/pull/5225.
                // - use a `CASE` statement on the chunk's type: if it's an event chunk, run an
                //   additional `SELECT` query. It was an immense improvement, but still caused
                //   one select query per event chunk. This happened in
                //   https://github.com/matrix-org/matrix-rust-sdk/pull/5411.
                //
                // The current solution is to run two queries:
                // - one to get each chunk and its number of events, by doing a single `SELECT`
                //   query over the `event_chunks` table, grouping by chunk ids. This gives us a
                //   list of `(chunk_id, num_events)` pairs, which can be transformed into a
                //   hashmap.
                // - one to get each chunk's metadata (id, previous, next, type) from the
                //   database with a `SELECT`, and then use the hashmap to get the number of
                //   events.
                //
                // This strategy minimizes the number of queries to the database, and keeps them
                // super simple, while doing a bit more processing here, which is much faster.

                let num_events_by_chunk_ids = txn
                    .prepare(
                        r#"
                            SELECT ec.chunk_id, COUNT(ec.event_id)
                            FROM event_chunks as ec
                            WHERE ec.linked_chunk_id = ?
                            GROUP BY ec.chunk_id
                        "#,
                    )?
                    .query_map((&hashed_linked_chunk_id,), |row| {
                        Ok((row.get::<_, u64>(0)?, row.get::<_, usize>(1)?))
                    })?
                    .collect::<Result<HashMap<_, _>, _>>()?;

                txn.prepare(
                    r#"
                        SELECT
                            lc.id,
                            lc.previous,
                            lc.next,
                            lc.type
                        FROM linked_chunks as lc
                        WHERE lc.linked_chunk_id = ?
                        ORDER BY lc.id"#,
                )?
                .query_map((&hashed_linked_chunk_id,), |row| {
                    Ok((
                        row.get::<_, u64>(0)?,
                        row.get::<_, Option<u64>>(1)?,
                        row.get::<_, Option<u64>>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })?
                .map(|data| -> Result<_> {
                    let (id, previous, next, chunk_type) = data?;

                    // Note: since a gap has 0 events, an alternative could be to *not* retrieve
                    // the chunk type, and just let the hashmap lookup fail for gaps. However,
                    // benchmarking shows that this is slightly slower than matching the chunk
                    // type (around 1%, so in the realm of noise), so we keep the explicit
                    // check instead.
                    let num_items = if chunk_type == CHUNK_TYPE_GAP_TYPE_STRING {
                        0
                    } else {
                        num_events_by_chunk_ids.get(&id).copied().unwrap_or(0)
                    };

                    Ok(ChunkMetadata {
                        identifier: ChunkIdentifier::new(id),
                        previous: previous.map(ChunkIdentifier::new),
                        next: next.map(ChunkIdentifier::new),
                        num_items,
                    })
                })
                .collect::<Result<Vec<_>, _>>()
            })
            .await
    }

    #[instrument(skip(self))]
    async fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator), Self::Error> {
        let _timer = timer!("method");

        let hashed_linked_chunk_id =
            self.encode_key(keys::LINKED_CHUNKS, linked_chunk_id.storage_key());

        let this = self.clone();

        self
            .read()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                // Find the latest chunk identifier to generate a `ChunkIdentifierGenerator`, and count the number of chunks.
                let (observed_max_identifier, number_of_chunks) = txn
                    .prepare(
                        "SELECT MAX(id), COUNT(*) FROM linked_chunks WHERE linked_chunk_id = ?"
                    )?
                    .query_row(
                        (&hashed_linked_chunk_id,),
                        |row| {
                            Ok((
                                // Read the `MAX(id)` as an `Option<u64>` instead
                                // of `u64` in case the `SELECT` returns nothing.
                                // Indeed, if it returns no line, the `MAX(id)` is
                                // set to `Null`.
                                row.get::<_, Option<u64>>(0)?,
                                row.get::<_, u64>(1)?,
                            ))
                        }
                    )?;

                let chunk_identifier_generator = match observed_max_identifier {
                    Some(max_observed_identifier) => {
                        ChunkIdentifierGenerator::new_from_previous_chunk_identifier(
                            ChunkIdentifier::new(max_observed_identifier)
                        )
                    },
                    None => ChunkIdentifierGenerator::new_from_scratch(),
                };

                // Find the last chunk.
                let Some((chunk_identifier, previous_chunk, chunk_type)) = txn
                    .prepare(
                        "SELECT id, previous, type FROM linked_chunks WHERE linked_chunk_id = ? AND next IS NULL"
                    )?
                    .query_row(
                        (&hashed_linked_chunk_id,),
                        |row| {
                            Ok((
                                row.get::<_, u64>(0)?,
                                row.get::<_, Option<u64>>(1)?,
                                row.get::<_, String>(2)?,
                            ))
                        }
                    )
                    .optional()?
                else {
                    // Chunk is not found and there are zero chunks for this room, this is consistent, all
                    // good.
                    if number_of_chunks == 0 {
                        return Ok((None, chunk_identifier_generator));
                    }
                    // Chunk is not found **but** there are chunks for this room, this is inconsistent. The
                    // linked chunk is malformed.
                    //
                    // Returning `Ok((None, _))` would be invalid here: we must return an error.
                    else {
                        return Err(Error::InvalidData {
                            details:
                                "last chunk is not found but chunks exist: the linked chunk contains a cycle"
                                    .to_owned()
                            }
                        )
                    }
                };

                // Build the chunk.
                let last_chunk = txn.rebuild_chunk(
                    &this,
                    &hashed_linked_chunk_id,
                    previous_chunk,
                    chunk_identifier,
                    None,
                    &chunk_type
                )?;

                Ok((Some(last_chunk), chunk_identifier_generator))
            })
            .await
    }

    #[instrument(skip(self))]
    async fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, Self::Error> {
        let _timer = timer!("method");

        let hashed_linked_chunk_id =
            self.encode_key(keys::LINKED_CHUNKS, linked_chunk_id.storage_key());

        let this = self.clone();

        self
            .read()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                // Find the chunk before the chunk identified by `before_chunk_identifier`.
                let Some((chunk_identifier, previous_chunk, next_chunk, chunk_type)) = txn
                    .prepare(
                        "SELECT id, previous, next, type FROM linked_chunks WHERE linked_chunk_id = ? AND next = ?"
                    )?
                    .query_row(
                        (&hashed_linked_chunk_id, before_chunk_identifier.index()),
                        |row| {
                            Ok((
                                row.get::<_, u64>(0)?,
                                row.get::<_, Option<u64>>(1)?,
                                row.get::<_, Option<u64>>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        }
                    )
                    .optional()?
                else {
                    // Chunk is not found.
                    return Ok(None);
                };

                // Build the chunk.
                let last_chunk = txn.rebuild_chunk(
                    &this,
                    &hashed_linked_chunk_id,
                    previous_chunk,
                    chunk_identifier,
                    next_chunk,
                    &chunk_type
                )?;

                Ok(Some(last_chunk))
            })
            .await
    }

    #[instrument(skip(self))]
    async fn clear_all_linked_chunks(&self) -> Result<(), Self::Error> {
        let _timer = timer!("method");

        self.write()
            .await?
            .with_transaction(move |txn| {
                // Remove all the chunks, and let cascading do its job.
                txn.execute("DELETE FROM linked_chunks", ())?;
                // Also clear all the events' contents.
                txn.execute("DELETE FROM events", ())
            })
            .await?;

        Ok(())
    }

    #[instrument(skip(self, events))]
    async fn filter_duplicated_events(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, Self::Error> {
        let _timer = timer!("method");

        // If there's no events for which we want to check duplicates, we can return
        // early. It's not only an optimization to do so: it's required, otherwise the
        // `repeat_vars` call below will panic.
        if events.is_empty() {
            return Ok(Vec::new());
        }

        // Select all events that exist in the store, i.e. the duplicates.
        let hashed_linked_chunk_id =
            self.encode_key(keys::LINKED_CHUNKS, linked_chunk_id.storage_key());
        let linked_chunk_id = linked_chunk_id.to_owned();

        self.read()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                txn.chunk_large_query_over(events, None, move |txn, events| {
                    let query = format!(
                        r#"
                            SELECT event_id, chunk_id, position
                            FROM event_chunks
                            WHERE linked_chunk_id = ? AND event_id IN ({})
                            ORDER BY chunk_id ASC, position ASC
                        "#,
                        repeat_vars(events.len()),
                    );

                    let parameters = params_from_iter(
                        // parameter for `linked_chunk_id = ?`
                        once(
                            hashed_linked_chunk_id
                                .to_sql()
                                // SAFETY: it cannot fail since `Key::to_sql` never fails
                                .unwrap(),
                        )
                        // parameters for `event_id IN (…)`
                        .chain(events.iter().map(|event| {
                            event
                                .as_str()
                                .to_sql()
                                // SAFETY: it cannot fail since `str::to_sql` never fails
                                .unwrap()
                        })),
                    );

                    let mut duplicated_events = Vec::new();

                    for duplicated_event in txn.prepare(&query)?.query_map(parameters, |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, u64>(1)?,
                            row.get::<_, usize>(2)?,
                        ))
                    })? {
                        let (duplicated_event, chunk_identifier, index) = duplicated_event?;

                        let Ok(duplicated_event) = EventId::parse(duplicated_event.clone()) else {
                            // Normally unreachable, but the event ID has been stored even if it is
                            // malformed, let's skip it.
                            error!(%duplicated_event, %linked_chunk_id, "Reading an malformed event ID");
                            continue;
                        };

                        duplicated_events.push((
                            duplicated_event,
                            Position::new(ChunkIdentifier::new(chunk_identifier), index),
                        ));
                    }

                    Ok(duplicated_events)
                })
            })
            .await
    }

    #[instrument(skip(self, event_id))]
    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, Self::Error> {
        let _timer = timer!("method");

        let event_id = event_id.to_owned();
        let this = self.clone();

        let hashed_room_id = self.encode_key(keys::LINKED_CHUNKS, room_id);

        self.read()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                let Some(event) = txn
                    .prepare("SELECT content FROM events WHERE event_id = ? AND room_id = ?")?
                    .query_row((event_id.as_str(), hashed_room_id), |row| row.get::<_, Vec<u8>>(0))
                    .optional()?
                else {
                    // Event is not found.
                    return Ok(None);
                };

                let event = serde_json::from_slice(&this.decode_value(&event)?)?;

                Ok(Some(event))
            })
            .await
    }

    #[instrument(skip(self, event_id, filters))]
    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filters: Option<&[RelationType]>,
    ) -> Result<Vec<(Event, Option<Position>)>, Self::Error> {
        let _timer = timer!("method");

        let hashed_room_id = self.encode_key(keys::LINKED_CHUNKS, room_id);

        let hashed_linked_chunk_id =
            self.encode_key(keys::LINKED_CHUNKS, LinkedChunkId::Room(room_id).storage_key());

        let event_id = event_id.to_owned();
        let filters = filters.map(ToOwned::to_owned);
        let store = self.clone();

        self.read()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                find_event_relations_transaction(
                    store,
                    hashed_room_id,
                    hashed_linked_chunk_id,
                    event_id,
                    filters,
                    txn,
                )
            })
            .await
    }

    #[instrument(skip(self))]
    async fn get_room_events(
        &self,
        room_id: &RoomId,
        event_type: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<Event>, Self::Error> {
        let _timer = timer!("method");

        let this = self.clone();

        let hashed_room_id = self.encode_key(keys::LINKED_CHUNKS, room_id);
        let hashed_event_type = event_type.map(|e| self.encode_key(keys::EVENTS, e));
        let hashed_session_id = session_id.map(|s| self.encode_key(keys::EVENTS, s));

        self.read()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                // I'm not sure why clippy claims that the clones aren't required. The compiler
                // tells us that the lifetimes aren't long enough if we remove them. Doesn't matter
                // much so let's silence things.
                #[allow(clippy::redundant_clone)]
                let (query, keys) = match (hashed_event_type, hashed_session_id) {
                    (None, None) => {
                        ("SELECT content FROM events WHERE room_id = ?", params![hashed_room_id])
                    }
                    (None, Some(session_id)) => (
                        "SELECT content FROM events WHERE room_id = ?1 AND session_id = ?2",
                        params![hashed_room_id, session_id.to_owned()],
                    ),
                    (Some(event_type), None) => (
                        "SELECT content FROM events WHERE room_id = ? AND event_type = ?",
                        params![hashed_room_id, event_type.to_owned()]
                    ),
                    (Some(event_type), Some(session_id)) => (
                        "SELECT content FROM events WHERE room_id = ?1 AND event_type = ?2 AND session_id = ?3",
                        params![hashed_room_id, event_type.to_owned(), session_id.to_owned()],
                    ),
                };

                let mut statement = txn.prepare(query)?;
                let maybe_events = statement.query_map(keys, |row| row.get::<_, Vec<u8>>(0))?;

                let mut events = Vec::new();
                for ev in maybe_events {
                    let event = serde_json::from_slice(&this.decode_value(&ev?)?)?;
                    events.push(event);
                }

                Ok(events)
            })
            .await
    }

    #[instrument(skip(self, event))]
    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error> {
        let _timer = timer!("method");

        let Some(event_id) = event.event_id() else {
            error!("Trying to save an event with no ID");
            return Ok(());
        };

        let Some(event_type) = event.kind.event_type() else {
            error!(%event_id, "Trying to save an event with no event type");
            return Ok(());
        };

        let event_type = self.encode_key(keys::EVENTS, event_type);
        let session_id = event.kind.session_id().map(|s| self.encode_key(keys::EVENTS, s));

        let hashed_room_id = self.encode_key(keys::LINKED_CHUNKS, room_id);
        let event_id = event_id.to_string();
        let encoded_event = self.encode_event(&event)?;

        self.write()
            .await?
            .with_transaction(move |txn| -> Result<_> {
                txn.execute(
                    "INSERT OR REPLACE INTO events(room_id, event_id, event_type, session_id, content, relates_to, rel_type) VALUES (?, ?, ?, ?, ?, ?, ?)",
                    (&hashed_room_id, &event_id, event_type, session_id, encoded_event.content, encoded_event.relates_to, encoded_event.rel_type))?;

                Ok(())
            })
            .await
    }

    async fn optimize(&self) -> Result<(), Self::Error> {
        Ok(self.vacuum().await?)
    }

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        self.get_db_size().await
    }
}

fn find_event_relations_transaction(
    store: SqliteEventCacheStore,
    hashed_room_id: Key,
    hashed_linked_chunk_id: Key,
    event_id: OwnedEventId,
    filters: Option<Vec<RelationType>>,
    txn: &Transaction<'_>,
) -> Result<Vec<(Event, Option<Position>)>> {
    let get_rows = |row: &rusqlite::Row<'_>| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Option<u64>>(1)?,
            row.get::<_, Option<usize>>(2)?,
        ))
    };

    // Collect related events.
    let collect_results = |transaction| {
        let mut related = Vec::new();

        for result in transaction {
            let (event_blob, chunk_id, index): (Vec<u8>, Option<u64>, _) = result?;

            let event: Event = serde_json::from_slice(&store.decode_value(&event_blob)?)?;

            // Only build the position if both the chunk_id and position were present; in
            // theory, they should either be present at the same time, or not at all.
            let pos = chunk_id
                .zip(index)
                .map(|(chunk_id, index)| Position::new(ChunkIdentifier::new(chunk_id), index));

            related.push((event, pos));
        }

        Ok(related)
    };

    if let Some(filters) = filters {
        let question_marks = repeat_vars(filters.len());
        let query = format!(
            "SELECT events.content, event_chunks.chunk_id, event_chunks.position
            FROM events
            LEFT JOIN event_chunks ON events.event_id = event_chunks.event_id AND event_chunks.linked_chunk_id = ?
            WHERE relates_to = ? AND room_id = ? AND rel_type IN ({question_marks})"
        );

        // First the filters need to be stringified; because `.to_sql()` will borrow
        // from them, they also need to be stringified onto the stack, so as to
        // get a stable address (to avoid returning a temporary reference in the
        // map closure below).
        let filter_strings: Vec<_> = filters.iter().map(|f| f.to_string()).collect();
        let filters_params: Vec<_> = filter_strings
            .iter()
            .map(|f| f.to_sql().expect("converting a string to SQL should work"))
            .collect();

        let parameters = params_from_iter(
            [
                hashed_linked_chunk_id.to_sql().expect(
                    "We should be able to convert a hashed linked chunk ID to a SQLite value",
                ),
                event_id
                    .as_str()
                    .to_sql()
                    .expect("We should be able to convert an event ID to a SQLite value"),
                hashed_room_id
                    .to_sql()
                    .expect("We should be able to convert a room ID to a SQLite value"),
            ]
            .into_iter()
            .chain(filters_params),
        );

        let mut transaction = txn.prepare(&query)?;
        let transaction = transaction.query_map(parameters, get_rows)?;

        collect_results(transaction)
    } else {
        let query =
            "SELECT events.content, event_chunks.chunk_id, event_chunks.position
            FROM events
            LEFT JOIN event_chunks ON events.event_id = event_chunks.event_id AND event_chunks.linked_chunk_id = ?
            WHERE relates_to = ? AND room_id = ?";
        let parameters = (hashed_linked_chunk_id, event_id.as_str(), hashed_room_id);

        let mut transaction = txn.prepare(query)?;
        let transaction = transaction.query_map(parameters, get_rows)?;

        collect_results(transaction)
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
    this: &SqliteEventCacheStore,
    f: F,
) -> Result<T, Error> {
    this.write()
        .await?
        .interact(move |conn| -> Result<T, Error> {
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
    linked_chunk_id: &Key,
    previous: Option<u64>,
    new: u64,
    next: Option<u64>,
    type_str: &str,
) -> rusqlite::Result<()> {
    // First, insert the new chunk.
    txn.execute(
        r#"
            INSERT INTO linked_chunks(id, linked_chunk_id, previous, next, type)
            VALUES (?, ?, ?, ?, ?)
        "#,
        (new, linked_chunk_id, previous, next, type_str),
    )?;

    // If this chunk has a previous one, update its `next` field.
    if let Some(previous) = previous {
        txn.execute(
            r#"
                UPDATE linked_chunks
                SET next = ?
                WHERE id = ? AND linked_chunk_id = ?
            "#,
            (new, previous, linked_chunk_id),
        )?;
    }

    // If this chunk has a next one, update its `previous` field.
    if let Some(next) = next {
        txn.execute(
            r#"
                UPDATE linked_chunks
                SET previous = ?
                WHERE id = ? AND linked_chunk_id = ?
            "#,
            (new, next, linked_chunk_id),
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicU32, Ordering::SeqCst},
    };

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        event_cache::{
            Gap,
            store::{
                EventCacheStore, EventCacheStoreError,
                integration_tests::{
                    check_test_event, make_test_event, make_test_event_with_event_id,
                },
            },
        },
        event_cache_store_integration_tests, event_cache_store_integration_tests_time,
        linked_chunk::{ChunkContent, ChunkIdentifier, LinkedChunkId, Position, Update},
    };
    use matrix_sdk_test::{DEFAULT_TEST_ROOM_ID, async_test};
    use once_cell::sync::Lazy;
    use ruma::{event_id, room_id};
    use tempfile::{TempDir, tempdir};

    use super::SqliteEventCacheStore;
    use crate::{
        SqliteStoreConfig,
        event_cache_store::keys,
        utils::{EncryptableStore as _, SqliteAsyncConnExt},
    };

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());
    static NUM: AtomicU32 = AtomicU32::new(0);

    fn new_event_cache_store_workspace() -> PathBuf {
        let name = NUM.fetch_add(1, SeqCst).to_string();
        TMP_DIR.path().join(name)
    }

    async fn get_event_cache_store() -> Result<SqliteEventCacheStore, EventCacheStoreError> {
        let tmpdir_path = new_event_cache_store_workspace();

        tracing::info!("using event cache store @ {}", tmpdir_path.to_str().unwrap());

        Ok(SqliteEventCacheStore::open(tmpdir_path.to_str().unwrap(), None).await.unwrap())
    }

    event_cache_store_integration_tests!();
    event_cache_store_integration_tests_time!();

    #[async_test]
    async fn test_pool_size() {
        let tmpdir_path = new_event_cache_store_workspace();
        let store_open_config = SqliteStoreConfig::new(tmpdir_path).pool_max_size(42);

        let store = SqliteEventCacheStore::open_with_config(store_open_config).await.unwrap();

        assert_eq!(store.pool.status().max_size, 42);
    }

    #[async_test]
    async fn test_linked_chunk_new_items_chunk() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
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

        let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();

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
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
                vec![Update::NewGapChunk {
                    previous: None,
                    new: ChunkIdentifier::new(42),
                    next: None,
                    gap: Gap { prev_token: "raclette".to_owned() },
                }],
            )
            .await
            .unwrap();

        let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();

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
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let event_id = event_id!("$world");

        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
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
                            make_test_event_with_event_id(room_id, "world", Some(event_id)),
                        ],
                    },
                    Update::ReplaceItem {
                        at: Position::new(ChunkIdentifier::new(42), 1),
                        item: make_test_event_with_event_id(room_id, "yolo", Some(event_id)),
                    },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();

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
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
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

        let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();

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

        // Check that cascading worked. Yes, SQLite, I doubt you.
        let gaps = store
            .read()
            .await
            .unwrap()
            .with_transaction(|txn| -> rusqlite::Result<_> {
                let mut gaps = Vec::new();
                for data in txn
                    .prepare("SELECT chunk_id FROM gap_chunks ORDER BY chunk_id")?
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
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
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

        let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();

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
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "one"),
                            make_test_event(room_id, "two"),
                            make_test_event(room_id, "three"),
                            make_test_event(room_id, "four"),
                            make_test_event(room_id, "five"),
                            make_test_event(room_id, "six"),
                        ],
                    },
                    Update::RemoveItem {
                        at: Position::new(ChunkIdentifier::new(42), 2), /* "three" */
                    },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();

        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 5);
            check_test_event(&events[0], "one");
            check_test_event(&events[1], "two");
            check_test_event(&events[2], "four");
            check_test_event(&events[3], "five");
            check_test_event(&events[4], "six");
        });

        // Make sure the position have been updated for the remaining events.
        let num_rows: u64 = store
            .read()
            .await
            .unwrap()
            .with_transaction(move |txn| {
                txn.query_row(
                    "SELECT COUNT(*) FROM event_chunks WHERE chunk_id = 42 AND linked_chunk_id = ? AND position IN (2, 3, 4)",
                    (store.encode_key(keys::LINKED_CHUNKS, linked_chunk_id.storage_key()),),
                    |row| row.get(0),
                )
            })
            .await
            .unwrap();
        assert_eq!(num_rows, 3);
    }

    #[async_test]
    async fn test_linked_chunk_detach_last_items() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
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

        let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();

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
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        // Same updates and checks as test_linked_chunk_push_items, but with extra
        // `StartReattachItems` and `EndReattachItems` updates, which must have no
        // effects.
        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
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

        let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();

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
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let event_0 = make_test_event(room_id, "hello");
        let event_1 = make_test_event(room_id, "world");
        let event_2 = make_test_event(room_id, "howdy");

        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
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
                        items: vec![event_0.clone(), event_1, event_2],
                    },
                    Update::Clear,
                ],
            )
            .await
            .unwrap();

        let chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
        assert!(chunks.is_empty());

        // Check that cascading worked. Yes, SQLite, I doubt you.
        store
            .read()
            .await
            .unwrap()
            .with_transaction(|txn| -> rusqlite::Result<_> {
                let num_gaps = txn
                    .prepare("SELECT COUNT(chunk_id) FROM gap_chunks ORDER BY chunk_id")?
                    .query_row((), |row| row.get::<_, u64>(0))?;
                assert_eq!(num_gaps, 0);

                let num_events = txn
                    .prepare("SELECT COUNT(event_id) FROM event_chunks ORDER BY chunk_id")?
                    .query_row((), |row| row.get::<_, u64>(0))?;
                assert_eq!(num_events, 0);

                Ok(())
            })
            .await
            .unwrap();

        // It's okay to re-insert a past event.
        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![event_0],
                    },
                ],
            )
            .await
            .unwrap();
    }

    #[async_test]
    async fn test_linked_chunk_multiple_rooms() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room1 = room_id!("!realcheeselovers:raclette.fr");
        let linked_chunk_id1 = LinkedChunkId::Room(room1);
        let room2 = room_id!("!realcheeselovers:fondue.ch");
        let linked_chunk_id2 = LinkedChunkId::Room(room2);

        // Check that applying updates to one room doesn't affect the others.
        // Use the same chunk identifier in both rooms to battle-test search.

        store
            .handle_linked_chunk_updates(
                linked_chunk_id1,
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
                linked_chunk_id2,
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
        let mut chunks_room1 = store.load_all_chunks(linked_chunk_id1).await.unwrap();
        assert_eq!(chunks_room1.len(), 1);

        let c = chunks_room1.remove(0);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 2);
            check_test_event(&events[0], "best cheese is raclette");
            check_test_event(&events[1], "obviously");
        });

        // Check chunks from room 2.
        let mut chunks_room2 = store.load_all_chunks(linked_chunk_id2).await.unwrap();
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
        let linked_chunk_id = LinkedChunkId::Room(room_id);

        // Trigger a violation of the unique constraint on the (room id, chunk id)
        // couple.
        let err = store
            .handle_linked_chunk_updates(
                linked_chunk_id,
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
        let chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
        assert!(chunks.is_empty());
    }

    #[async_test]
    async fn test_filter_duplicate_events_no_events() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let duplicates = store.filter_duplicated_events(linked_chunk_id, Vec::new()).await.unwrap();
        assert!(duplicates.is_empty());
    }

    #[async_test]
    async fn test_load_last_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let event = |msg: &str| make_test_event(room_id, msg);
        let store = get_event_cache_store().await.expect("creating cache store failed");

        // Case #1: no last chunk.
        {
            let (last_chunk, chunk_identifier_generator) =
                store.load_last_chunk(linked_chunk_id).await.unwrap();

            assert!(last_chunk.is_none());
            assert_eq!(chunk_identifier_generator.current(), 0);
        }

        // Case #2: only one chunk is present.
        {
            store
                .handle_linked_chunk_updates(
                    linked_chunk_id,
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(42),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(42), 0),
                            items: vec![event("saucisse de morteau"), event("comté")],
                        },
                    ],
                )
                .await
                .unwrap();

            let (last_chunk, chunk_identifier_generator) =
                store.load_last_chunk(linked_chunk_id).await.unwrap();

            assert_matches!(last_chunk, Some(last_chunk) => {
                assert_eq!(last_chunk.identifier, 42);
                assert!(last_chunk.previous.is_none());
                assert!(last_chunk.next.is_none());
                assert_matches!(last_chunk.content, ChunkContent::Items(items) => {
                    assert_eq!(items.len(), 2);
                    check_test_event(&items[0], "saucisse de morteau");
                    check_test_event(&items[1], "comté");
                });
            });
            assert_eq!(chunk_identifier_generator.current(), 42);
        }

        // Case #3: more chunks are present.
        {
            store
                .handle_linked_chunk_updates(
                    linked_chunk_id,
                    vec![
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(42)),
                            new: ChunkIdentifier::new(7),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(7), 0),
                            items: vec![event("fondue"), event("gruyère"), event("mont d'or")],
                        },
                    ],
                )
                .await
                .unwrap();

            let (last_chunk, chunk_identifier_generator) =
                store.load_last_chunk(linked_chunk_id).await.unwrap();

            assert_matches!(last_chunk, Some(last_chunk) => {
                assert_eq!(last_chunk.identifier, 7);
                assert_matches!(last_chunk.previous, Some(previous) => {
                    assert_eq!(previous, 42);
                });
                assert!(last_chunk.next.is_none());
                assert_matches!(last_chunk.content, ChunkContent::Items(items) => {
                    assert_eq!(items.len(), 3);
                    check_test_event(&items[0], "fondue");
                    check_test_event(&items[1], "gruyère");
                    check_test_event(&items[2], "mont d'or");
                });
            });
            assert_eq!(chunk_identifier_generator.current(), 42);
        }
    }

    #[async_test]
    async fn test_load_last_chunk_with_a_cycle() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let store = get_event_cache_store().await.expect("creating cache store failed");

        store
            .handle_linked_chunk_updates(
                linked_chunk_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::NewItemsChunk {
                        // Because `previous` connects to chunk #0, it will create a cycle.
                        // Chunk #0 will have a `next` set to chunk #1! Consequently, the last chunk
                        // **does not exist**. We have to detect this cycle.
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: Some(ChunkIdentifier::new(0)),
                    },
                ],
            )
            .await
            .unwrap();

        store.load_last_chunk(linked_chunk_id).await.unwrap_err();
    }

    #[async_test]
    async fn test_load_previous_chunk() {
        let room_id = room_id!("!r0:matrix.org");
        let linked_chunk_id = LinkedChunkId::Room(room_id);
        let event = |msg: &str| make_test_event(room_id, msg);
        let store = get_event_cache_store().await.expect("creating cache store failed");

        // Case #1: no chunk at all, equivalent to having an nonexistent
        // `before_chunk_identifier`.
        {
            let previous_chunk = store
                .load_previous_chunk(linked_chunk_id, ChunkIdentifier::new(153))
                .await
                .unwrap();

            assert!(previous_chunk.is_none());
        }

        // Case #2: there is one chunk only: we request the previous on this
        // one, it doesn't exist.
        {
            store
                .handle_linked_chunk_updates(
                    linked_chunk_id,
                    vec![Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    }],
                )
                .await
                .unwrap();

            let previous_chunk =
                store.load_previous_chunk(linked_chunk_id, ChunkIdentifier::new(42)).await.unwrap();

            assert!(previous_chunk.is_none());
        }

        // Case #3: there are two chunks.
        {
            store
                .handle_linked_chunk_updates(
                    linked_chunk_id,
                    vec![
                        // new chunk before the one that exists.
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(7),
                            next: Some(ChunkIdentifier::new(42)),
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(7), 0),
                            items: vec![event("brigand du jorat"), event("morbier")],
                        },
                    ],
                )
                .await
                .unwrap();

            let previous_chunk =
                store.load_previous_chunk(linked_chunk_id, ChunkIdentifier::new(42)).await.unwrap();

            assert_matches!(previous_chunk, Some(previous_chunk) => {
                assert_eq!(previous_chunk.identifier, 7);
                assert!(previous_chunk.previous.is_none());
                assert_matches!(previous_chunk.next, Some(next) => {
                    assert_eq!(next, 42);
                });
                assert_matches!(previous_chunk.content, ChunkContent::Items(items) => {
                    assert_eq!(items.len(), 2);
                    check_test_event(&items[0], "brigand du jorat");
                    check_test_event(&items[1], "morbier");
                });
            });
        }
    }

    #[async_test]
    async fn test_event_chunks_allows_same_event_in_room_and_thread() {
        // This test verifies that the same event can appear in both a room's linked
        // chunk and a thread's linked chunk. This is the real-world use case:
        // a thread reply appears in both the main room timeline and the thread.

        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;
        let thread_root = event_id!("$thread_root");

        // Create an event that will be inserted into both the room and thread linked
        // chunks.
        let event = make_test_event_with_event_id(
            room_id,
            "thread reply",
            Some(event_id!("$thread_reply")),
        );

        let room_linked_chunk_id = LinkedChunkId::Room(room_id);
        let thread_linked_chunk_id = LinkedChunkId::Thread(room_id, thread_root);

        // Insert the event into the room's linked chunk.
        store
            .handle_linked_chunk_updates(
                room_linked_chunk_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![event.clone()],
                    },
                ],
            )
            .await
            .unwrap();

        // Insert the same event into the thread's linked chunk.
        store
            .handle_linked_chunk_updates(
                thread_linked_chunk_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![event],
                    },
                ],
            )
            .await
            .unwrap();

        // Verify both entries exist by loading chunks from both linked chunk IDs.
        let room_chunks = store.load_all_chunks(room_linked_chunk_id).await.unwrap();
        let thread_chunks = store.load_all_chunks(thread_linked_chunk_id).await.unwrap();

        assert_eq!(room_chunks.len(), 1);
        assert_eq!(thread_chunks.len(), 1);

        // Verify the event is in both.
        assert_matches!(&room_chunks[0].content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_id().as_deref(), Some(event_id!("$thread_reply")));
        });
        assert_matches!(&thread_chunks[0].content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_id().as_deref(), Some(event_id!("$thread_reply")));
        });
    }
}

#[cfg(test)]
mod encrypted_tests {
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    use matrix_sdk_base::{
        event_cache::store::{EventCacheStore, EventCacheStoreError},
        event_cache_store_integration_tests, event_cache_store_integration_tests_time,
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use once_cell::sync::Lazy;
    use ruma::{
        event_id,
        events::{relation::RelationType, room::message::RoomMessageEventContentWithoutRelation},
        room_id, user_id,
    };
    use tempfile::{TempDir, tempdir};

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

    #[async_test]
    async fn test_no_sqlite_injection_in_find_event_relations() {
        let room_id = room_id!("!test:localhost");
        let another_room_id = room_id!("!r1:matrix.org");
        let sender = user_id!("@alice:localhost");

        let store = get_event_cache_store()
            .await
            .expect("We should be able to create a new, empty, event cache store");

        let f = EventFactory::new().room(room_id).sender(sender);

        // Create an event for the first room.
        let event_id = event_id!("$DO_NOT_FIND_ME:matrix.org");
        let event = f.text_msg("DO NOT FIND").event_id(event_id).into_event();

        // Create a related event.
        let edit_id = event_id!("$find_me:matrix.org");
        let edit = f
            .text_msg("Find me")
            .event_id(edit_id)
            .edit(event_id, RoomMessageEventContentWithoutRelation::text_plain("jebote"))
            .into_event();

        // Create an event for the second room.
        let f = f.room(another_room_id);

        let another_event_id = event_id!("$DO_NOT_FIND_ME_EITHER:matrix.org");
        let another_event =
            f.text_msg("DO NOT FIND ME EITHER").event_id(another_event_id).into_event();

        // Save the events in the DB.
        store.save_event(room_id, event).await.unwrap();
        store.save_event(room_id, edit).await.unwrap();
        store.save_event(another_room_id, another_event).await.unwrap();

        // Craft a `RelationType` that will inject some SQL to be executed. The
        // `OR 1=1` ensures that all the previous parameters, the room
        // ID and event ID are ignored.
        let filter = Some(vec![RelationType::Replacement, "x\") OR 1=1; --".into()]);

        // Attempt to find events in the first room.
        let results = store
            .find_event_relations(room_id, event_id, filter.as_deref())
            .await
            .expect("We should be able to attempt to find event relations");

        // Ensure that we only got the single related event the first room contains.
        similar_asserts::assert_eq!(
            results.len(),
            1,
            "We should only have loaded events for the first room {results:#?}"
        );

        // The event needs to be the edit event, otherwise something is wrong.
        let (found_event, _) = &results[0];
        assert_eq!(
            found_event.event_id().as_deref(),
            Some(edit_id),
            "The single event we found should be the edit event"
        );
    }
}
