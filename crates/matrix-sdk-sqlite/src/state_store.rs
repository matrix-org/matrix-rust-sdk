use std::{
    borrow::Cow,
    cmp::min,
    collections::{BTreeMap, BTreeSet},
    fmt,
    future::Future,
    iter,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use deadpool_sqlite::{Object as SqliteConn, Pool as SqlitePool, Runtime};
use itertools::Itertools;
use matrix_sdk_base::{
    deserialized_responses::RawAnySyncOrStrippedState,
    media::{MediaRequest, UniqueKey},
    MinimalRoomMemberEvent, RoomInfo, RoomMemberships, RoomState, StateChanges, StateStore,
    StateStoreDataKey, StateStoreDataValue,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    canonical_json::{redact, RedactedBecause},
    events::{
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::member::{StrippedRoomMemberEvent, SyncRoomMemberEvent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncStateEvent,
        GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    serde::Raw,
    CanonicalJsonObject, EventId, OwnedEventId, OwnedUserId, RoomId, RoomVersionId, UserId,
};
use rusqlite::{limits::Limit, OptionalExtension, Transaction};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::fs;
use tracing::{debug, warn};

use crate::{
    error::{Error, Result},
    get_or_create_store_cipher,
    utils::{load_db_version, Key, SqliteObjectExt},
    OpenStoreError, SqliteObjectStoreExt,
};

mod keys {
    // Tables
    pub const KV_BLOB: &str = "kv_blob";
    pub const ROOM_INFO: &str = "room_info";
    pub const STATE_EVENT: &str = "state_event";
    pub const GLOBAL_ACCOUNT_DATA: &str = "global_account_data";
    pub const ROOM_ACCOUNT_DATA: &str = "room_account_data";
    pub const MEMBER: &str = "member";
    pub const PROFILE: &str = "profile";
    pub const RECEIPT: &str = "receipt";
    pub const DISPLAY_NAME: &str = "display_name";
    pub const MEDIA: &str = "media";
}

const DATABASE_VERSION: u8 = 2;

/// A sqlite based cryptostore.
#[derive(Clone)]
pub struct SqliteStateStore {
    store_cipher: Option<Arc<StoreCipher>>,
    path: Option<PathBuf>,
    pool: SqlitePool,
}

impl fmt::Debug for SqliteStateStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(path) = &self.path {
            f.debug_struct("SqliteStateStore").field("path", &path).finish()
        } else {
            f.debug_struct("SqliteStateStore").field("path", &"memory store").finish()
        }
    }
}

impl SqliteStateStore {
    /// Open the sqlite-based state store at the given path using the given
    /// passphrase to encrypt private data.
    pub async fn open(
        path: impl AsRef<Path>,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        let pool = create_pool(path.as_ref()).await?;

        Self::open_with_pool(pool, passphrase).await
    }

    /// Create a sqlite-based state store using the given sqlite database pool.
    /// The given passphrase will be used to encrypt private data.
    pub async fn open_with_pool(
        pool: SqlitePool,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        let conn = pool.get().await?;
        let mut version = load_db_version(&conn).await?;

        if version == 0 {
            init(&conn).await?;
            version = 1;
        }

        let store_cipher = match passphrase {
            Some(p) => Some(Arc::new(get_or_create_store_cipher(p, &conn).await?)),
            None => None,
        };
        let this = Self { store_cipher, path: None, pool };
        this.run_migrations(&conn, version, None).await?;

        Ok(this)
    }

    /// Run database migrations from the given `from` version to the given `to`
    /// version
    ///
    /// If `to` is `None`, the current database version will be used.
    async fn run_migrations(&self, conn: &SqliteConn, from: u8, to: Option<u8>) -> Result<()> {
        let to = to.unwrap_or(DATABASE_VERSION);

        if from < to {
            debug!(version = from, new_version = to, "Upgrading database");
        } else {
            return Ok(());
        }

        if from < 2 && to >= 2 {
            let this = self.clone();
            conn.with_transaction(move |txn| {
                // Create new table.
                txn.execute_batch(include_str!(
                    "../migrations/state_store/002_a_create_new_room_info.sql"
                ))?;

                // Migrate data to new table.
                for data in txn
                    .prepare("SELECT data FROM room_info")?
                    .query_map((), |row| row.get::<_, Vec<u8>>(0))?
                {
                    let data = data?;
                    let room_info: RoomInfo = this.deserialize_json(&data)?;

                    let room_id = this.encode_key(keys::ROOM_INFO, room_info.room_id());
                    let state = this
                        .encode_key(keys::ROOM_INFO, serde_json::to_string(&room_info.state())?);
                    txn.prepare_cached(
                        "INSERT OR REPLACE INTO new_room_info (room_id, state, data)
                         VALUES (?, ?, ?)",
                    )?
                    .execute((room_id, state, data))?;
                }

                // Replace old table.
                txn.execute_batch(include_str!(
                    "../migrations/state_store/002_b_replace_room_info.sql"
                ))?;

                Result::<_, Error>::Ok(())
            })
            .await?;
        }

        conn.set_kv("version", vec![to]).await?;

        Ok(())
    }

    fn encode_value(&self, value: Vec<u8>) -> Result<Vec<u8>> {
        if let Some(key) = &self.store_cipher {
            let encrypted = key.encrypt_value_data(value)?;
            Ok(rmp_serde::to_vec_named(&encrypted)?)
        } else {
            Ok(value)
        }
    }

    fn serialize_value(&self, value: &impl Serialize) -> Result<Vec<u8>> {
        let serialized = rmp_serde::to_vec_named(value)?;
        self.encode_value(serialized)
    }

    fn serialize_json(&self, value: &impl Serialize) -> Result<Vec<u8>> {
        let serialized = serde_json::to_vec(value)?;
        self.encode_value(serialized)
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

    fn deserialize_json<T: DeserializeOwned>(&self, data: &[u8]) -> Result<T> {
        let decoded = self.decode_value(data)?;
        Ok(serde_json::from_slice(&decoded)?)
    }

    fn deserialize_value<T: DeserializeOwned>(&self, value: &[u8]) -> Result<T> {
        let decoded = self.decode_value(value)?;
        Ok(rmp_serde::from_slice(&decoded)?)
    }

    fn encode_key(&self, table_name: &str, key: impl AsRef<[u8]>) -> Key {
        let bytes = key.as_ref();
        if let Some(store_cipher) = &self.store_cipher {
            Key::Hashed(store_cipher.hash_key(table_name, bytes))
        } else {
            Key::Plain(bytes.to_owned())
        }
    }

    fn encode_state_store_data_key(&self, key: StateStoreDataKey<'_>) -> Key {
        let key_s = match key {
            StateStoreDataKey::SyncToken => Cow::Borrowed(StateStoreDataKey::SYNC_TOKEN),
            StateStoreDataKey::Filter(f) => {
                Cow::Owned(format!("{}:{f}", StateStoreDataKey::FILTER))
            }
            StateStoreDataKey::UserAvatarUrl(u) => {
                Cow::Owned(format!("{}:{u}", StateStoreDataKey::USER_AVATAR_URL))
            }
        };

        self.encode_key(keys::KV_BLOB, &*key_s)
    }

    fn encode_presence_key(&self, user_id: &UserId) -> Key {
        self.encode_key(keys::KV_BLOB, format!("presence:{user_id}"))
    }

    fn encode_custom_key(&self, key: &[u8]) -> Key {
        let mut full_key = b"custom:".to_vec();
        full_key.extend(key);
        self.encode_key(keys::KV_BLOB, full_key)
    }

    async fn acquire(&self) -> Result<deadpool_sqlite::Object> {
        Ok(self.pool.get().await?)
    }

    fn remove_maybe_stripped_room_data(
        &self,
        txn: &Transaction<'_>,
        room_id: &RoomId,
        stripped: bool,
    ) -> rusqlite::Result<()> {
        let state_event_room_id = self.encode_key(keys::STATE_EVENT, room_id);
        txn.remove_room_state_events(&state_event_room_id, Some(stripped))?;

        let member_room_id = self.encode_key(keys::MEMBER, room_id);
        txn.remove_room_members(&member_room_id, Some(stripped))
    }
}

async fn create_pool(path: &Path) -> Result<SqlitePool, OpenStoreError> {
    fs::create_dir_all(path).await.map_err(OpenStoreError::CreateDir)?;
    let cfg = deadpool_sqlite::Config::new(path.join("matrix-sdk-state.sqlite3"));
    Ok(cfg.create_pool(Runtime::Tokio1)?)
}

/// Initialize the database.
async fn init(conn: &SqliteConn) -> Result<()> {
    // First turn on WAL mode, this can't be done in the transaction, it fails with
    // the error message: "cannot change into wal mode from within a transaction".
    conn.execute_batch("PRAGMA journal_mode = wal;").await?;
    conn.with_transaction(|txn| {
        txn.execute_batch(include_str!("../migrations/state_store/001_init.sql"))
    })
    .await?;

    conn.set_kv("version", vec![1]).await?;

    Ok(())
}

trait SqliteConnectionStateStoreExt {
    fn set_kv_blob(&self, key: &[u8], value: &[u8]) -> rusqlite::Result<()>;

    fn set_global_account_data(&self, event_type: &[u8], data: &[u8]) -> rusqlite::Result<()>;

    fn set_room_account_data(
        &self,
        room_id: &[u8],
        event_type: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()>;
    fn remove_room_account_data(&self, room_id: &[u8]) -> rusqlite::Result<()>;

    fn set_room_info(&self, room_id: &[u8], state: &[u8], data: &[u8]) -> rusqlite::Result<()>;
    fn get_room_info(&self, room_id: &[u8]) -> rusqlite::Result<Option<Vec<u8>>>;
    fn remove_room_info(&self, room_id: &[u8]) -> rusqlite::Result<()>;

    fn set_state_event(
        &self,
        room_id: &[u8],
        event_type: &[u8],
        state_key: &[u8],
        stripped: bool,
        event_id: Option<&[u8]>,
        data: &[u8],
    ) -> rusqlite::Result<()>;
    fn get_state_event_by_id(
        &self,
        room_id: &[u8],
        event_id: &[u8],
    ) -> rusqlite::Result<Option<Vec<u8>>>;
    fn remove_room_state_events(
        &self,
        room_id: &[u8],
        stripped: Option<bool>,
    ) -> rusqlite::Result<()>;

    fn set_member(
        &self,
        room_id: &[u8],
        user_id: &[u8],
        membership: &[u8],
        stripped: bool,
        data: &[u8],
    ) -> rusqlite::Result<()>;
    fn remove_room_members(&self, room_id: &[u8], stripped: Option<bool>) -> rusqlite::Result<()>;

    fn set_profile(&self, room_id: &[u8], user_id: &[u8], data: &[u8]) -> rusqlite::Result<()>;
    fn remove_room_profiles(&self, room_id: &[u8]) -> rusqlite::Result<()>;

    fn set_receipt(
        &self,
        room_id: &[u8],
        user_id: &[u8],
        receipt_type: &[u8],
        thread_id: &[u8],
        event_id: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()>;
    fn remove_room_receipts(&self, room_id: &[u8]) -> rusqlite::Result<()>;

    fn set_display_name(&self, room_id: &[u8], name: &[u8], data: &[u8]) -> rusqlite::Result<()>;
    fn remove_display_name(&self, room_id: &[u8], name: &[u8]) -> rusqlite::Result<()>;
    fn remove_room_display_names(&self, room_id: &[u8]) -> rusqlite::Result<()>;
}

impl SqliteConnectionStateStoreExt for rusqlite::Connection {
    fn set_kv_blob(&self, key: &[u8], value: &[u8]) -> rusqlite::Result<()> {
        self.execute("INSERT OR REPLACE INTO kv_blob VALUES (?, ?)", (key, value))?;
        Ok(())
    }

    fn set_global_account_data(&self, event_type: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT OR REPLACE INTO global_account_data (event_type, data)
             VALUES (?, ?)",
        )?
        .execute((event_type, data))?;
        Ok(())
    }

    fn set_room_account_data(
        &self,
        room_id: &[u8],
        event_type: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT OR REPLACE INTO room_account_data (room_id, event_type, data)
             VALUES (?, ?, ?)",
        )?
        .execute((room_id, event_type, data))?;
        Ok(())
    }

    fn remove_room_account_data(&self, room_id: &[u8]) -> rusqlite::Result<()> {
        self.prepare(
            "DELETE FROM room_account_data
             WHERE room_id = ?",
        )?
        .execute((room_id,))?;
        Ok(())
    }

    fn set_room_info(&self, room_id: &[u8], state: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT OR REPLACE INTO room_info (room_id, state, data)
             VALUES (?, ?, ?)",
        )?
        .execute((room_id, state, data))?;
        Ok(())
    }

    fn get_room_info(&self, room_id: &[u8]) -> rusqlite::Result<Option<Vec<u8>>> {
        self.query_row("SELECT data FROM room_info WHERE room_id = ?", (room_id,), |row| row.get(0))
            .optional()
    }

    /// Remove the room info for the given room.
    fn remove_room_info(&self, room_id: &[u8]) -> rusqlite::Result<()> {
        self.prepare_cached("DELETE FROM room_info WHERE room_id = ?")?.execute((room_id,))?;
        Ok(())
    }

    fn set_state_event(
        &self,
        room_id: &[u8],
        event_type: &[u8],
        state_key: &[u8],
        stripped: bool,
        event_id: Option<&[u8]>,
        data: &[u8],
    ) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT OR REPLACE
             INTO state_event (room_id, event_type, state_key, stripped, event_id, data)
             VALUES (?, ?, ?, ?, ?, ?)",
        )?
        .execute((room_id, event_type, state_key, stripped, event_id, data))?;
        Ok(())
    }

    fn get_state_event_by_id(
        &self,
        room_id: &[u8],
        event_id: &[u8],
    ) -> rusqlite::Result<Option<Vec<u8>>> {
        self.query_row(
            "SELECT data FROM state_event WHERE room_id = ? AND event_id = ?",
            (room_id, event_id),
            |row| row.get(0),
        )
        .optional()
    }

    /// Remove state events for the given room.
    ///
    /// If `stripped` is `Some()`, only removes state events for the given
    /// stripped state. Otherwise, state events are removed regardless of the
    /// stripped state.
    fn remove_room_state_events(
        &self,
        room_id: &[u8],
        stripped: Option<bool>,
    ) -> rusqlite::Result<()> {
        if let Some(stripped) = stripped {
            self.prepare_cached("DELETE FROM state_event WHERE room_id = ? AND stripped = ?")?
                .execute((room_id, stripped))?;
        } else {
            self.prepare_cached("DELETE FROM state_event WHERE room_id = ?")?
                .execute((room_id,))?;
        }
        Ok(())
    }

    fn set_member(
        &self,
        room_id: &[u8],
        user_id: &[u8],
        membership: &[u8],
        stripped: bool,
        data: &[u8],
    ) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT OR REPLACE
             INTO member (room_id, user_id, membership, stripped, data)
             VALUES (?, ?, ?, ?, ?)",
        )?
        .execute((room_id, user_id, membership, stripped, data))?;
        Ok(())
    }

    /// Remove members for the given room.
    ///
    /// If `stripped` is `Some()`, only removes members for the given stripped
    /// state. Otherwise, members are removed regardless of the stripped state.
    fn remove_room_members(&self, room_id: &[u8], stripped: Option<bool>) -> rusqlite::Result<()> {
        if let Some(stripped) = stripped {
            self.prepare_cached("DELETE FROM member WHERE room_id = ? AND stripped = ?")?
                .execute((room_id, stripped))?;
        } else {
            self.prepare_cached("DELETE FROM member WHERE room_id = ?")?.execute((room_id,))?;
        }
        Ok(())
    }

    fn set_profile(&self, room_id: &[u8], user_id: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT OR REPLACE
             INTO profile (room_id, user_id, data)
             VALUES (?, ?, ?)",
        )?
        .execute((room_id, user_id, data))?;
        Ok(())
    }

    fn remove_room_profiles(&self, room_id: &[u8]) -> rusqlite::Result<()> {
        self.prepare("DELETE FROM profile WHERE room_id = ?")?.execute((room_id,))?;
        Ok(())
    }

    fn set_receipt(
        &self,
        room_id: &[u8],
        user_id: &[u8],
        receipt_type: &[u8],
        thread: &[u8],
        event_id: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT OR REPLACE
             INTO receipt (room_id, user_id, receipt_type, thread, event_id, data)
             VALUES (?, ?, ?, ?, ?, ?)",
        )?
        .execute((room_id, user_id, receipt_type, thread, event_id, data))?;
        Ok(())
    }

    fn remove_room_receipts(&self, room_id: &[u8]) -> rusqlite::Result<()> {
        self.prepare("DELETE FROM receipt WHERE room_id = ?")?.execute((room_id,))?;
        Ok(())
    }

    fn set_display_name(&self, room_id: &[u8], name: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT OR REPLACE
             INTO display_name (room_id, name, data)
             VALUES (?, ?, ?)",
        )?
        .execute((room_id, name, data))?;
        Ok(())
    }

    fn remove_display_name(&self, room_id: &[u8], name: &[u8]) -> rusqlite::Result<()> {
        self.prepare("DELETE FROM display_name WHERE room_id = ? AND name = ?")?
            .execute((room_id, name))?;
        Ok(())
    }

    fn remove_room_display_names(&self, room_id: &[u8]) -> rusqlite::Result<()> {
        self.prepare("DELETE FROM display_name WHERE room_id = ?")?.execute((room_id,))?;
        Ok(())
    }
}

#[async_trait]
trait SqliteObjectStateStoreExt: SqliteObjectExt {
    async fn get_kv_blob(&self, key: Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row("SELECT value FROM kv_blob WHERE key = ?", (key,), |row| row.get(0))
            .await
            .optional()?)
    }

    async fn get_kv_blobs(&self, keys: Vec<Key>) -> Result<Vec<Vec<u8>>> {
        let keys_length = keys.len();

        chunk_large_query_over(keys, Some(keys_length), |keys| {
            let sql_params = repeat_vars(keys.len());
            let sql = format!("SELECT value FROM kv_blob WHERE key IN ({sql_params})");

            let params = rusqlite::params_from_iter(keys);

            self.prepare(sql, move |mut stmt| {
                stmt.query(params)?.mapped(|row| row.get(0)).collect()
            })
        })
        .await
    }

    async fn set_kv_blob(&self, key: Key, value: Vec<u8>) -> Result<()>;

    async fn delete_kv_blob(&self, key: Key) -> Result<()> {
        self.execute("DELETE FROM kv_blob WHERE key = ?", (key,)).await?;
        Ok(())
    }

    async fn get_room_infos(&self, states: Vec<Key>) -> Result<Vec<Vec<u8>>> {
        if states.is_empty() {
            Ok(self
                .prepare("SELECT data FROM room_info", move |mut stmt| {
                    stmt.query_map((), |row| row.get(0))?.collect()
                })
                .await?)
        } else {
            chunk_large_query_over(states, None, |states| {
                let sql_params = repeat_vars(states.len());
                let sql = format!("SELECT data FROM room_info WHERE state IN ({sql_params})");

                self.prepare(sql, move |mut stmt| {
                    stmt.query(rusqlite::params_from_iter(states))?
                        .mapped(|row| row.get(0))
                        .collect()
                })
            })
            .await
        }
    }

    async fn get_maybe_stripped_state_events_for_keys(
        &self,
        room_id: Key,
        event_type: Key,
        state_keys: Vec<Key>,
    ) -> Result<Vec<(bool, Vec<u8>)>> {
        chunk_large_query_over(state_keys, None, move |state_keys: Vec<Key>| {
            let sql_params = repeat_vars(state_keys.len());
            let sql = format!(
                "SELECT stripped, data FROM state_event
                 WHERE room_id = ? AND event_type = ? AND state_key IN ({sql_params})"
            );

            let params = rusqlite::params_from_iter(
                [room_id.clone(), event_type.clone()].into_iter().chain(state_keys),
            );

            self.prepare(sql, |mut stmt| {
                stmt.query(params)?.mapped(|row| Ok((row.get(0)?, row.get(1)?))).collect()
            })
        })
        .await
    }

    async fn get_maybe_stripped_state_events(
        &self,
        room_id: Key,
        event_type: Key,
    ) -> Result<Vec<(bool, Vec<u8>)>> {
        Ok(self
            .prepare(
                "SELECT stripped, data FROM state_event
                 WHERE room_id = ? AND event_type = ?",
                |mut stmt| {
                    stmt.query((room_id, event_type))?
                        .mapped(|row| Ok((row.get(0)?, row.get(1)?)))
                        .collect()
                },
            )
            .await?)
    }

    async fn get_profiles(
        &self,
        room_id: Key,
        user_ids: Vec<Key>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let user_ids_length = user_ids.len();

        chunk_large_query_over(user_ids, Some(user_ids_length), move |user_ids| {
            let sql_params = repeat_vars(user_ids.len());
            let sql = format!(
                "SELECT user_id, data FROM profile WHERE room_id = ? AND user_id IN ({sql_params})"
            );

            let params = rusqlite::params_from_iter(iter::once(room_id.clone()).chain(user_ids));

            self.prepare(sql, move |mut stmt| {
                stmt.query(params)?.mapped(|row| Ok((row.get(0)?, row.get(1)?))).collect()
            })
        })
        .await
    }

    async fn get_user_ids(&self, room_id: Key, memberships: Vec<Key>) -> Result<Vec<Vec<u8>>> {
        let res = if memberships.is_empty() {
            self.prepare("SELECT data FROM member WHERE room_id = ?", |mut stmt| {
                stmt.query((room_id,))?.mapped(|row| row.get(0)).collect()
            })
            .await?
        } else {
            chunk_large_query_over(memberships, None, move |memberships| {
                let sql_params = repeat_vars(memberships.len());
                let sql = format!(
                    "SELECT data FROM member WHERE room_id = ? AND membership IN ({sql_params})"
                );

                let params =
                    rusqlite::params_from_iter(iter::once(room_id.clone()).chain(memberships));

                self.prepare(sql, move |mut stmt| {
                    stmt.query(params)?.mapped(|row| row.get(0)).collect()
                })
            })
            .await?
        };

        Ok(res)
    }

    async fn get_global_account_data(&self, event_type: Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row(
                "SELECT data FROM global_account_data WHERE event_type = ?",
                (event_type,),
                |row| row.get(0),
            )
            .await
            .optional()?)
    }

    async fn get_room_account_data(
        &self,
        room_id: Key,
        event_type: Key,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row(
                "SELECT data FROM room_account_data WHERE room_id = ? AND event_type = ?",
                (room_id, event_type),
                |row| row.get(0),
            )
            .await
            .optional()?)
    }

    async fn get_display_names(
        &self,
        room_id: Key,
        names: Vec<Key>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let names_length = names.len();

        chunk_large_query_over(names, Some(names_length), move |names| {
            let sql_params = repeat_vars(names.len());
            let sql = format!(
                "SELECT name, data FROM display_name WHERE room_id = ? AND name IN ({sql_params})"
            );

            let params = rusqlite::params_from_iter(iter::once(room_id.clone()).chain(names));

            self.prepare(sql, move |mut stmt| {
                stmt.query(params)?.mapped(|row| Ok((row.get(0)?, row.get(1)?))).collect()
            })
        })
        .await
    }

    async fn get_user_receipt(
        &self,
        room_id: Key,
        receipt_type: Key,
        thread: Key,
        user_id: Key,
    ) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row(
                "SELECT data FROM receipt
                 WHERE room_id = ? AND receipt_type = ? AND thread = ? and user_id = ?",
                (room_id, receipt_type, thread, user_id),
                |row| row.get(0),
            )
            .await
            .optional()?)
    }

    async fn get_event_receipts(
        &self,
        room_id: Key,
        receipt_type: Key,
        thread: Key,
        event_id: Key,
    ) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare(
                "SELECT data FROM receipt
                 WHERE room_id = ? AND receipt_type = ? AND thread = ? and event_id = ?",
                |mut stmt| {
                    stmt.query((room_id, receipt_type, thread, event_id))?
                        .mapped(|row| row.get(0))
                        .collect()
                },
            )
            .await?)
    }

    async fn set_media(&self, uri: Key, format: Key, data: Vec<u8>) -> Result<()> {
        self.execute(
            "INSERT OR REPLACE INTO media (uri, format, data) VALUES (?, ?, ?)",
            (uri, format, data),
        )
        .await?;
        Ok(())
    }

    async fn get_media(&self, uri: Key, format: Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row(
                "SELECT data FROM media WHERE uri = ? AND format = ?",
                (uri, format),
                |row| row.get(0),
            )
            .await
            .optional()?)
    }

    async fn remove_media(&self, uri: Key, format: Key) -> Result<()> {
        self.execute("DELETE FROM media WHERE uri = ? AND format = ?", (uri, format)).await?;
        Ok(())
    }

    async fn remove_uri_medias(&self, uri: Key) -> Result<()> {
        self.execute("DELETE FROM media WHERE uri = ?", (uri,)).await?;
        Ok(())
    }
}

#[async_trait]
impl SqliteObjectStateStoreExt for deadpool_sqlite::Object {
    async fn set_kv_blob(&self, key: Key, value: Vec<u8>) -> Result<()> {
        Ok(self.interact(move |conn| conn.set_kv_blob(&key, &value)).await.unwrap()?)
    }
}

#[async_trait]
impl StateStore for SqliteStateStore {
    type Error = Error;

    async fn get_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<Option<StateStoreDataValue>> {
        self.acquire()
            .await?
            .get_kv_blob(self.encode_state_store_data_key(key))
            .await?
            .map(|data| {
                let string = self.deserialize_value(&data)?;
                Ok(match key {
                    StateStoreDataKey::SyncToken => StateStoreDataValue::SyncToken(string),
                    StateStoreDataKey::Filter(_) => StateStoreDataValue::Filter(string),
                    StateStoreDataKey::UserAvatarUrl(_) => {
                        StateStoreDataValue::UserAvatarUrl(string)
                    }
                })
            })
            .transpose()
    }

    async fn set_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
        value: StateStoreDataValue,
    ) -> Result<()> {
        let value = match key {
            StateStoreDataKey::SyncToken => {
                value.into_sync_token().expect("Session data not a sync token")
            }
            StateStoreDataKey::Filter(_) => value.into_filter().expect("Session data not a filter"),
            StateStoreDataKey::UserAvatarUrl(_) => {
                value.into_user_avatar_url().expect("Session data not an user avatar url")
            }
        };

        self.acquire()
            .await?
            .set_kv_blob(self.encode_state_store_data_key(key), self.serialize_value(&value)?)
            .await
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<()> {
        self.acquire().await?.delete_kv_blob(self.encode_state_store_data_key(key)).await
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let changes = changes.to_owned();
        let this = self.clone();
        self.acquire()
            .await?
            .with_transaction(move |txn| {
                let StateChanges {
                    sync_token,
                    account_data,
                    presence,
                    profiles,
                    state,
                    room_account_data,
                    room_infos,
                    receipts,
                    redactions,
                    stripped_state,
                    ambiguity_maps,
                    notifications: _,
                } = changes;

                if let Some(sync_token) = sync_token {
                    let key = this.encode_state_store_data_key(StateStoreDataKey::SyncToken);
                    let value = this.serialize_value(&sync_token)?;
                    txn.set_kv_blob(&key, &value)?;
                }

                for (event_type, event) in account_data {
                    let event_type =
                        this.encode_key(keys::GLOBAL_ACCOUNT_DATA, event_type.to_string());
                    let data = this.serialize_json(&event)?;
                    txn.set_global_account_data(&event_type, &data)?;
                }

                for (room_id, events) in room_account_data {
                    let room_id = this.encode_key(keys::ROOM_ACCOUNT_DATA, room_id);
                    for (event_type, event) in events {
                        let event_type =
                            this.encode_key(keys::ROOM_ACCOUNT_DATA, event_type.to_string());
                        let data = this.serialize_json(&event)?;
                        txn.set_room_account_data(&room_id, &event_type, &data)?;
                    }
                }

                for (user_id, event) in presence {
                    let key = this.encode_presence_key(&user_id);
                    let value = this.serialize_json(&event)?;
                    txn.set_kv_blob(&key, &value)?;
                }

                for (room_id, room_info) in room_infos {
                    let stripped = room_info.state() == RoomState::Invited;
                    // Remove non-stripped data for stripped rooms and vice-versa.
                    this.remove_maybe_stripped_room_data(txn, &room_id, !stripped)?;

                    let room_id = this.encode_key(keys::ROOM_INFO, room_id);
                    let state = this
                        .encode_key(keys::ROOM_INFO, serde_json::to_string(&room_info.state())?);
                    let data = this.serialize_json(&room_info)?;
                    txn.set_room_info(&room_id, &state, &data)?;
                }

                for (room_id, state_event_types) in state {
                    let profiles = profiles.get(&room_id);
                    let encoded_room_id = this.encode_key(keys::STATE_EVENT, &room_id);

                    for (event_type, state_events) in state_event_types {
                        let encoded_event_type =
                            this.encode_key(keys::STATE_EVENT, event_type.to_string());

                        for (state_key, raw_state_event) in state_events {
                            let encoded_state_key = this.encode_key(keys::STATE_EVENT, &state_key);
                            let data = this.serialize_json(&raw_state_event)?;

                            let event_id: Option<String> =
                                raw_state_event.get_field("event_id").ok().flatten();
                            let encoded_event_id =
                                event_id.as_ref().map(|e| this.encode_key(keys::STATE_EVENT, e));

                            txn.set_state_event(
                                &encoded_room_id,
                                &encoded_event_type,
                                &encoded_state_key,
                                false,
                                encoded_event_id.as_deref(),
                                &data,
                            )?;

                            if event_type == StateEventType::RoomMember {
                                let member_event = match raw_state_event
                                    .deserialize_as::<SyncRoomMemberEvent>()
                                {
                                    Ok(ev) => ev,
                                    Err(e) => {
                                        debug!(event_id, "Failed to deserialize member event: {e}");
                                        continue;
                                    }
                                };

                                let encoded_room_id = this.encode_key(keys::MEMBER, &room_id);
                                let user_id = this.encode_key(keys::MEMBER, &state_key);
                                let membership = this
                                    .encode_key(keys::MEMBER, member_event.membership().as_str());
                                let data = this.serialize_value(&state_key)?;

                                txn.set_member(
                                    &encoded_room_id,
                                    &user_id,
                                    &membership,
                                    false,
                                    &data,
                                )?;

                                if let Some(profile) =
                                    profiles.and_then(|p| p.get(member_event.state_key()))
                                {
                                    let room_id = this.encode_key(keys::PROFILE, &room_id);
                                    let user_id = this.encode_key(keys::PROFILE, &state_key);
                                    let data = this.serialize_json(&profile)?;
                                    txn.set_profile(&room_id, &user_id, &data)?;
                                }
                            }
                        }
                    }
                }

                for (room_id, stripped_state_event_types) in stripped_state {
                    let encoded_room_id = this.encode_key(keys::STATE_EVENT, &room_id);

                    for (event_type, stripped_state_events) in stripped_state_event_types {
                        let encoded_event_type =
                            this.encode_key(keys::STATE_EVENT, event_type.to_string());

                        for (state_key, raw_stripped_state_event) in stripped_state_events {
                            let encoded_state_key = this.encode_key(keys::STATE_EVENT, &state_key);
                            let data = this.serialize_json(&raw_stripped_state_event)?;
                            txn.set_state_event(
                                &encoded_room_id,
                                &encoded_event_type,
                                &encoded_state_key,
                                true,
                                None,
                                &data,
                            )?;

                            if event_type == StateEventType::RoomMember {
                                let member_event = match raw_stripped_state_event
                                    .deserialize_as::<StrippedRoomMemberEvent>(
                                ) {
                                    Ok(ev) => ev,
                                    Err(e) => {
                                        debug!("Failed to deserialize stripped member event: {e}");
                                        continue;
                                    }
                                };

                                let room_id = this.encode_key(keys::MEMBER, &room_id);
                                let user_id = this.encode_key(keys::MEMBER, &state_key);
                                let membership = this.encode_key(
                                    keys::MEMBER,
                                    member_event.content.membership.as_str(),
                                );
                                let data = this.serialize_value(&state_key)?;

                                txn.set_member(&room_id, &user_id, &membership, true, &data)?;
                            }
                        }
                    }
                }

                for (room_id, receipt_event) in receipts {
                    let room_id = this.encode_key(keys::RECEIPT, room_id);

                    for (event_id, receipt_types) in receipt_event {
                        let encoded_event_id = this.encode_key(keys::RECEIPT, &event_id);

                        for (receipt_type, receipt_users) in receipt_types {
                            let receipt_type =
                                this.encode_key(keys::RECEIPT, receipt_type.as_str());

                            for (user_id, receipt) in receipt_users {
                                let encoded_user_id = this.encode_key(keys::RECEIPT, &user_id);
                                // We cannot have a NULL primary key so we rely on serialization
                                // instead of the string representation.
                                let thread = this.encode_key(
                                    keys::RECEIPT,
                                    rmp_serde::to_vec_named(&receipt.thread)?,
                                );
                                let data = this.serialize_json(&ReceiptData {
                                    receipt,
                                    event_id: event_id.clone(),
                                    user_id,
                                })?;

                                txn.set_receipt(
                                    &room_id,
                                    &encoded_user_id,
                                    &receipt_type,
                                    &thread,
                                    &encoded_event_id,
                                    &data,
                                )?;
                            }
                        }
                    }
                }

                for (room_id, redactions) in redactions {
                    let make_room_version = || {
                        let encoded_room_id = this.encode_key(keys::ROOM_INFO, &room_id);
                        txn.get_room_info(&encoded_room_id)
                            .ok()
                            .flatten()
                            .and_then(|v| this.deserialize_json::<RoomInfo>(&v).ok())
                            .and_then(|info| info.room_version().cloned())
                            .unwrap_or_else(|| {
                                warn!(
                                    ?room_id,
                                    "Unable to find the room version, assume version 9"
                                );
                                RoomVersionId::V9
                            })
                    };

                    let encoded_room_id = this.encode_key(keys::STATE_EVENT, &room_id);
                    let mut room_version = None;

                    for (event_id, redaction) in redactions {
                        let event_id = this.encode_key(keys::STATE_EVENT, event_id);

                        if let Some(Ok(raw_event)) = txn
                            .get_state_event_by_id(&encoded_room_id, &event_id)?
                            .map(|value| this.deserialize_json::<Raw<AnySyncStateEvent>>(&value))
                        {
                            let event = raw_event.deserialize()?;
                            let redacted = redact(
                                raw_event.deserialize_as::<CanonicalJsonObject>()?,
                                room_version.get_or_insert_with(make_room_version),
                                Some(RedactedBecause::from_raw_event(&redaction)?),
                            )
                            .map_err(Error::Redaction)?;
                            let data = this.serialize_json(&redacted)?;

                            let event_type =
                                this.encode_key(keys::STATE_EVENT, event.event_type().to_string());
                            let state_key = this.encode_key(keys::STATE_EVENT, event.state_key());

                            txn.set_state_event(
                                &encoded_room_id,
                                &event_type,
                                &state_key,
                                false,
                                Some(&event_id),
                                &data,
                            )?;
                        }
                    }
                }

                for (room_id, display_names) in ambiguity_maps {
                    let room_id = this.encode_key(keys::DISPLAY_NAME, room_id);

                    for (name, user_ids) in display_names {
                        let name = this.encode_key(keys::DISPLAY_NAME, name);
                        let data = this.serialize_json(&user_ids)?;

                        if user_ids.is_empty() {
                            txn.remove_display_name(&room_id, &name)?;
                        } else {
                            txn.set_display_name(&room_id, &name, &data)?;
                        }
                    }
                }

                Ok::<_, Error>(())
            })
            .await?;

        Ok(())
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        self.acquire()
            .await?
            .get_kv_blob(self.encode_presence_key(user_id))
            .await?
            .map(|data| self.deserialize_json(&data))
            .transpose()
    }

    async fn get_presence_events(
        &self,
        user_ids: &[OwnedUserId],
    ) -> Result<Vec<Raw<PresenceEvent>>> {
        if user_ids.is_empty() {
            return Ok(Vec::new());
        }

        let user_ids = user_ids.iter().map(|u| self.encode_presence_key(u)).collect();
        self.acquire()
            .await?
            .get_kv_blobs(user_ids)
            .await?
            .into_iter()
            .map(|data| self.deserialize_json(&data))
            .collect()
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<RawAnySyncOrStrippedState>> {
        Ok(self
            .get_state_events_for_keys(room_id, event_type, &[state_key])
            .await?
            .into_iter()
            .next())
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        let room_id = self.encode_key(keys::STATE_EVENT, room_id);
        let event_type = self.encode_key(keys::STATE_EVENT, event_type.to_string());
        self.acquire()
            .await?
            .get_maybe_stripped_state_events(room_id, event_type)
            .await?
            .into_iter()
            .map(|(stripped, data)| {
                let ev = if stripped {
                    RawAnySyncOrStrippedState::Stripped(self.deserialize_json(&data)?)
                } else {
                    RawAnySyncOrStrippedState::Sync(self.deserialize_json(&data)?)
                };

                Ok(ev)
            })
            .collect()
    }

    async fn get_state_events_for_keys(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>, Self::Error> {
        if state_keys.is_empty() {
            return Ok(Vec::new());
        }

        let room_id = self.encode_key(keys::STATE_EVENT, room_id);
        let event_type = self.encode_key(keys::STATE_EVENT, event_type.to_string());
        let state_keys = state_keys.iter().map(|k| self.encode_key(keys::STATE_EVENT, k)).collect();
        self.acquire()
            .await?
            .get_maybe_stripped_state_events_for_keys(room_id, event_type, state_keys)
            .await?
            .into_iter()
            .map(|(stripped, data)| {
                let ev = if stripped {
                    RawAnySyncOrStrippedState::Stripped(self.deserialize_json(&data)?)
                } else {
                    RawAnySyncOrStrippedState::Sync(self.deserialize_json(&data)?)
                };

                Ok(ev)
            })
            .collect()
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>> {
        let room_id = self.encode_key(keys::PROFILE, room_id);
        let user_ids = vec![self.encode_key(keys::PROFILE, user_id)];

        self.acquire()
            .await?
            .get_profiles(room_id, user_ids)
            .await?
            .into_iter()
            .next()
            .map(|(_, data)| self.deserialize_json(&data))
            .transpose()
    }

    async fn get_profiles<'a>(
        &self,
        room_id: &RoomId,
        user_ids: &'a [OwnedUserId],
    ) -> Result<BTreeMap<&'a UserId, MinimalRoomMemberEvent>> {
        if user_ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let room_id = self.encode_key(keys::PROFILE, room_id);
        let mut user_ids_map = user_ids
            .iter()
            .map(|u| (self.encode_key(keys::PROFILE, u), u.as_ref()))
            .collect::<BTreeMap<_, _>>();
        let user_ids = user_ids_map.keys().cloned().collect();

        self.acquire()
            .await?
            .get_profiles(room_id, user_ids)
            .await?
            .into_iter()
            .map(|(user_id, data)| {
                Ok((
                    user_ids_map
                        .remove(user_id.as_slice())
                        .expect("returned user IDs were requested"),
                    self.deserialize_json(&data)?,
                ))
            })
            .collect()
    }

    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        membership: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>> {
        let room_id = self.encode_key(keys::MEMBER, room_id);
        let memberships = membership
            .as_vec()
            .into_iter()
            .map(|m| self.encode_key(keys::MEMBER, m.as_str()))
            .collect();
        self.acquire()
            .await?
            .get_user_ids(room_id, memberships)
            .await?
            .iter()
            .map(|data| self.deserialize_value(data))
            .collect()
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        self.get_user_ids(room_id, RoomMemberships::INVITE).await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        self.get_user_ids(room_id, RoomMemberships::JOIN).await
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.acquire()
            .await?
            .get_room_infos(Vec::new())
            .await?
            .into_iter()
            .map(|data| self.deserialize_json(&data))
            .collect()
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        let states =
            vec![self.encode_key(keys::ROOM_INFO, serde_json::to_string(&RoomState::Invited)?)];
        self.acquire()
            .await?
            .get_room_infos(states)
            .await?
            .into_iter()
            .map(|data| self.deserialize_json(&data))
            .collect()
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>> {
        let room_id = self.encode_key(keys::DISPLAY_NAME, room_id);
        let names = vec![self.encode_key(keys::DISPLAY_NAME, display_name)];

        Ok(self
            .acquire()
            .await?
            .get_display_names(room_id, names)
            .await?
            .into_iter()
            .next()
            .map(|(_, data)| self.deserialize_json(&data))
            .transpose()?
            .unwrap_or_default())
    }

    async fn get_users_with_display_names<'a>(
        &self,
        room_id: &RoomId,
        display_names: &'a [String],
    ) -> Result<BTreeMap<&'a str, BTreeSet<OwnedUserId>>> {
        if display_names.is_empty() {
            return Ok(BTreeMap::new());
        }

        let room_id = self.encode_key(keys::DISPLAY_NAME, room_id);
        let mut names_map = display_names
            .iter()
            .map(|n| (self.encode_key(keys::DISPLAY_NAME, n), n.as_ref()))
            .collect::<BTreeMap<_, _>>();
        let names = names_map.keys().cloned().collect();

        self.acquire()
            .await?
            .get_display_names(room_id, names)
            .await?
            .into_iter()
            .map(|(name, data)| {
                Ok((
                    names_map
                        .remove(name.as_slice())
                        .expect("returned display names were requested"),
                    self.deserialize_json(&data)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        let event_type = self.encode_key(keys::GLOBAL_ACCOUNT_DATA, event_type.to_string());
        self.acquire()
            .await?
            .get_global_account_data(event_type)
            .await?
            .map(|value| self.deserialize_json(&value))
            .transpose()
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        let room_id = self.encode_key(keys::ROOM_ACCOUNT_DATA, room_id);
        let event_type = self.encode_key(keys::ROOM_ACCOUNT_DATA, event_type.to_string());
        self.acquire()
            .await?
            .get_room_account_data(room_id, event_type)
            .await?
            .map(|value| self.deserialize_json(&value))
            .transpose()
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        let room_id = self.encode_key(keys::RECEIPT, room_id);
        let receipt_type = self.encode_key(keys::RECEIPT, receipt_type.to_string());
        // We cannot have a NULL primary key so we rely on serialization instead of the
        // string representation.
        let thread = self.encode_key(keys::RECEIPT, rmp_serde::to_vec_named(&thread)?);
        let user_id = self.encode_key(keys::RECEIPT, user_id);

        self.acquire()
            .await?
            .get_user_receipt(room_id, receipt_type, thread, user_id)
            .await?
            .map(|value| {
                self.deserialize_json::<ReceiptData>(&value).map(|d| (d.event_id, d.receipt))
            })
            .transpose()
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        let room_id = self.encode_key(keys::RECEIPT, room_id);
        let receipt_type = self.encode_key(keys::RECEIPT, receipt_type.to_string());
        // We cannot have a NULL primary key so we rely on serialization instead of the
        // string representation.
        let thread = self.encode_key(keys::RECEIPT, rmp_serde::to_vec_named(&thread)?);
        let event_id = self.encode_key(keys::RECEIPT, event_id);

        self.acquire()
            .await?
            .get_event_receipts(room_id, receipt_type, thread, event_id)
            .await?
            .iter()
            .map(|value| {
                self.deserialize_json::<ReceiptData>(value).map(|d| (d.user_id, d.receipt))
            })
            .collect()
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.acquire().await?.get_kv_blob(self.encode_custom_key(key)).await
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let conn = self.acquire().await?;
        let key = self.encode_custom_key(key);
        let previous = conn.get_kv_blob(key.clone()).await?;
        conn.set_kv_blob(key, value).await?;
        Ok(previous)
    }

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let conn = self.acquire().await?;
        let key = self.encode_custom_key(key);
        let previous = conn.get_kv_blob(key.clone()).await?;
        if previous.is_some() {
            conn.delete_kv_blob(key).await?;
        }
        Ok(previous)
    }

    async fn add_media_content(&self, request: &MediaRequest, content: Vec<u8>) -> Result<()> {
        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());
        let data = self.encode_value(content)?;
        self.acquire().await?.set_media(uri, format, data).await
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());
        let data = self.acquire().await?.get_media(uri, format).await?;
        data.map(|v| self.decode_value(&v).map(Into::into)).transpose()
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        let uri = self.encode_key(keys::MEDIA, request.source.unique_key());
        let format = self.encode_key(keys::MEDIA, request.format.unique_key());
        self.acquire().await?.remove_media(uri, format).await
    }

    async fn remove_media_content_for_uri(&self, uri: &ruma::MxcUri) -> Result<()> {
        let uri = self.encode_key(keys::MEDIA, uri);
        self.acquire().await?.remove_uri_medias(uri).await
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        let this = self.clone();
        let room_id = room_id.to_owned();

        self.acquire()
            .await?
            .with_transaction(move |txn| {
                let room_info_room_id = this.encode_key(keys::ROOM_INFO, &room_id);
                txn.remove_room_info(&room_info_room_id)?;

                let state_event_room_id = this.encode_key(keys::STATE_EVENT, &room_id);
                txn.remove_room_state_events(&state_event_room_id, None)?;

                let member_room_id = this.encode_key(keys::MEMBER, &room_id);
                txn.remove_room_members(&member_room_id, None)?;

                let profile_room_id = this.encode_key(keys::PROFILE, &room_id);
                txn.remove_room_profiles(&profile_room_id)?;

                let room_account_data_room_id = this.encode_key(keys::ROOM_ACCOUNT_DATA, &room_id);
                txn.remove_room_account_data(&room_account_data_room_id)?;

                let receipt_room_id = this.encode_key(keys::RECEIPT, &room_id);
                txn.remove_room_receipts(&receipt_room_id)?;

                let display_name_room_id = this.encode_key(keys::DISPLAY_NAME, &room_id);
                txn.remove_room_display_names(&display_name_room_id)?;

                Ok(())
            })
            .await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReceiptData {
    receipt: Receipt,
    event_id: OwnedEventId,
    user_id: OwnedUserId,
}

/// Chunk a large query over some keys.
///
/// Imagine there is a _dynamic_ query that runs potentially large number of
/// parameters, so much that the maximum number of parameters can be hit. Then,
/// this helper is for you. It will execute the query on chunks of parameters.
async fn chunk_large_query_over<Query, Fut, Res>(
    mut keys_to_chunk: Vec<Key>,
    result_capacity: Option<usize>,
    do_query: Query,
) -> Result<Vec<Res>>
where
    Query: Fn(Vec<Key>) -> Fut,
    Fut: Future<Output = Result<Vec<Res>, rusqlite::Error>>,
{
    let mut all_results = if let Some(capacity) = result_capacity {
        Vec::with_capacity(capacity)
    } else {
        Vec::new()
    };

    // `Limit` has a `repr(i32)`, it's safe to cast it to `i32`. Then divide by 2 to
    // let space for more static parameters (not part of `keys_to_chunk`).
    let maximum_chunk_size = Limit::SQLITE_LIMIT_VARIABLE_NUMBER as i32 / 2;
    let maximum_chunk_size: usize = maximum_chunk_size
        .try_into()
        .map_err(|_| Error::SqliteMaximumVariableNumber(maximum_chunk_size))?;

    while !keys_to_chunk.is_empty() {
        let tail = keys_to_chunk.split_off(min(keys_to_chunk.len(), maximum_chunk_size));
        let chunk = keys_to_chunk;
        keys_to_chunk = tail;

        all_results.extend(do_query(chunk).await?);
    }

    Ok(all_results)
}

/// Repeat `?` n times, where n is defined by `count`. `?` are comma-separated.
fn repeat_vars(count: usize) -> impl fmt::Display {
    assert_ne!(count, 0);

    iter::repeat("?").take(count).format(",")
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    use matrix_sdk_base::{statestore_integration_tests, StateStore, StoreError};
    use once_cell::sync::Lazy;
    use tempfile::{tempdir, TempDir};

    use super::SqliteStateStore;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());
    static NUM: AtomicU32 = AtomicU32::new(0);

    async fn get_store() -> Result<impl StateStore, StoreError> {
        let name = NUM.fetch_add(1, SeqCst).to_string();
        let tmpdir_path = TMP_DIR.path().join(name);

        Ok(SqliteStateStore::open(tmpdir_path.to_str().unwrap(), None).await.unwrap())
    }

    statestore_integration_tests!(with_media_tests);
}

#[cfg(test)]
mod encrypted_tests {
    use std::sync::atomic::{AtomicU32, Ordering::SeqCst};

    use matrix_sdk_base::{statestore_integration_tests, StateStore, StoreError};
    use once_cell::sync::Lazy;
    use tempfile::{tempdir, TempDir};

    use super::SqliteStateStore;

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());
    static NUM: AtomicU32 = AtomicU32::new(0);

    async fn get_store() -> Result<impl StateStore, StoreError> {
        let name = NUM.fetch_add(1, SeqCst).to_string();
        let tmpdir_path = TMP_DIR.path().join(name);

        Ok(SqliteStateStore::open(tmpdir_path.to_str().unwrap(), Some("default_test_password"))
            .await
            .unwrap())
    }

    statestore_integration_tests!(with_media_tests);
}

#[cfg(test)]
mod migration_tests {
    use std::{
        path::{Path, PathBuf},
        sync::{
            atomic::{AtomicU32, Ordering::SeqCst},
            Arc,
        },
    };

    use matrix_sdk_base::{RoomInfo, RoomState, StateStore};
    use matrix_sdk_test::async_test;
    use once_cell::sync::Lazy;
    use ruma::RoomId;
    use tempfile::{tempdir, TempDir};

    use super::{create_pool, init, keys, SqliteStateStore};
    use crate::{
        error::{Error, Result},
        get_or_create_store_cipher,
        utils::SqliteObjectExt,
    };

    static TMP_DIR: Lazy<TempDir> = Lazy::new(|| tempdir().unwrap());
    static NUM: AtomicU32 = AtomicU32::new(0);
    const SECRET: &str = "secret";

    fn new_path() -> PathBuf {
        let name = NUM.fetch_add(1, SeqCst).to_string();
        TMP_DIR.path().join(name)
    }

    async fn create_fake_db(path: &Path, version: u8) -> Result<SqliteStateStore> {
        let pool = create_pool(path).await.unwrap();
        let conn = pool.get().await?;

        init(&conn).await?;

        let store_cipher = Some(Arc::new(get_or_create_store_cipher(SECRET, &conn).await.unwrap()));
        let this = SqliteStateStore { store_cipher, path: None, pool };
        this.run_migrations(&conn, 1, Some(version)).await?;

        Ok(this)
    }

    #[async_test]
    pub async fn test_migrating_v1_to_v2() {
        let path = new_path();
        // Create and populate db.
        {
            let db = create_fake_db(&path, 1).await.unwrap();
            let conn = db.pool.get().await.unwrap();

            let this = db.clone();
            conn.with_transaction(move |txn| {
                for i in 0..5 {
                    let room_id = RoomId::parse(format!("!room_{i}:localhost")).unwrap();
                    let (state, stripped) =
                        if i < 3 { (RoomState::Joined, false) } else { (RoomState::Invited, true) };
                    let info = RoomInfo::new(&room_id, state);

                    let room_id = this.encode_key(keys::ROOM_INFO, room_id);
                    let data = this.serialize_json(&info)?;

                    txn.prepare_cached(
                        "INSERT OR REPLACE INTO room_info (room_id, stripped, data)
                         VALUES (?, ?, ?)",
                    )?
                    .execute((room_id, stripped, data))?;
                }

                Result::<_, Error>::Ok(())
            })
            .await
            .unwrap();
        }

        // This transparently migrates to the latest version.
        let store = SqliteStateStore::open(path, Some(SECRET)).await.unwrap();

        // Check all room infos are there.
        assert_eq!(store.get_room_infos().await.unwrap().len(), 5);
        #[allow(deprecated)]
        let stripped_rooms = store.get_stripped_room_infos().await.unwrap();
        assert_eq!(stripped_rooms.len(), 2);
    }
}
