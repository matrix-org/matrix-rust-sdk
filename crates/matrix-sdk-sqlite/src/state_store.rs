use std::{
    borrow::Cow,
    collections::BTreeSet,
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use deadpool_sqlite::{Object as SqliteConn, Pool as SqlitePool, Runtime};
use matrix_sdk_base::{
    deserialized_responses::RawMemberEvent, media::MediaRequest, RoomInfo, RoomState, StateChanges,
    StateStore, StateStoreDataKey, StateStoreDataValue,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncStateEvent,
        GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    serde::Raw,
    EventId, OwnedUserId, RoomId, UserId,
};
use rusqlite::OptionalExtension;
use serde::{de::DeserializeOwned, Serialize};
use tokio::fs;
use tracing::{debug, error};

use crate::{
    error::{Error, Result},
    get_or_create_store_cipher,
    utils::{chain, Key, SqliteConnectionExt, SqliteObjectExt},
    OpenStoreError, SqliteObjectStoreExt,
};

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
    /// Open the sqlite-based crypto store at the given path using the given
    /// passphrase to encrypt private data.
    pub async fn open(
        path: impl AsRef<Path>,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        let path = path.as_ref();
        fs::create_dir_all(path).await.map_err(OpenStoreError::CreateDir)?;
        let cfg = deadpool_sqlite::Config::new(path.join("matrix-sdk-crypto.sqlite3"));
        let pool = cfg.create_pool(Runtime::Tokio1)?;

        Self::open_with_pool(pool, passphrase).await
    }

    /// Create a sqlite-based crypto store using the given sqlite database pool.
    /// The given passphrase will be used to encrypt private data.
    pub async fn open_with_pool(
        pool: SqlitePool,
        passphrase: Option<&str>,
    ) -> Result<Self, OpenStoreError> {
        let conn = pool.get().await?;
        run_migrations(&conn).await.map_err(OpenStoreError::Migration)?;
        let store_cipher = match passphrase {
            Some(p) => Some(Arc::new(get_or_create_store_cipher(p, &conn).await?)),
            None => None,
        };

        Ok(Self { store_cipher, path: None, pool })
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

        self.encode_key("kv_blob", &*key_s)
    }

    fn encode_presence_key(&self, user_id: &UserId) -> Key {
        self.encode_key("kv_blob", format!("presence:{user_id}"))
    }

    fn encode_custom_key(&self, key: &[u8]) -> Key {
        let mut full_key = b"custom:".to_vec();
        full_key.extend(key);
        self.encode_key("kv_blob", full_key)
    }

    async fn acquire(&self) -> Result<deadpool_sqlite::Object> {
        Ok(self.pool.get().await?)
    }
}

const DATABASE_VERSION: u8 = 1;

async fn run_migrations(conn: &SqliteConn) -> rusqlite::Result<()> {
    let kv_exists = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'kv'",
            (),
            |row| row.get::<_, u32>(0),
        )
        .await?
        > 0;

    let version = if kv_exists {
        match conn.get_kv("version").await?.as_deref() {
            Some([v]) => *v,
            Some(_) => {
                error!("version database field has multiple bytes");
                return Ok(());
            }
            None => {
                error!("version database field is missing");
                return Ok(());
            }
        }
    } else {
        0
    };

    if version == 0 {
        debug!("Creating database");
    } else if version < DATABASE_VERSION {
        debug!(version, new_version = DATABASE_VERSION, "Upgrading database");
    }

    if version < 1 {
        // First turn on WAL mode, this can't be done in the transaction, it fails with
        // the error message: "cannot change into wal mode from within a transaction".
        conn.execute_batch("PRAGMA journal_mode = wal;").await?;
        conn.with_transaction(|txn| {
            txn.execute_batch(include_str!("../migrations/state_store/001_init.sql"))
        })
        .await?;
    }

    conn.set_kv("version", vec![DATABASE_VERSION]).await?;

    Ok(())
}

trait SqliteConnectionStateStoreExt {
    fn set_kv_blob(&self, key: &[u8], value: &[u8]) -> rusqlite::Result<()>;

    fn set_room_info(&self, room_id: &[u8], stripped: bool, data: &[u8]) -> rusqlite::Result<()>;

    fn set_profile(
        &self,
        session_id: &[u8],
        sender_key: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()>;

    fn set_global_account_data(&self, event_type: &[u8], data: &[u8]) -> rusqlite::Result<()>;
    fn set_room_account_data(
        &self,
        room_id: &[u8],
        event_type: &[u8],
        data: &[u8],
    ) -> rusqlite::Result<()>;
}

impl SqliteConnectionStateStoreExt for rusqlite::Connection {
    fn set_kv_blob(&self, key: &[u8], value: &[u8]) -> rusqlite::Result<()> {
        self.execute(
            "INSERT INTO kv_blob VALUES (?1, ?2) ON CONFLICT (key) DO UPDATE SET value = ?2",
            (key, value),
        )?;
        Ok(())
    }

    fn set_room_info(&self, room_id: &[u8], stripped: bool, data: &[u8]) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT INTO room_info (room_id, stripped, data)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (room_id) DO UPDATE SET data = ?3",
        )?
        .execute((room_id, stripped, data))?;
        Ok(())
    }

    fn set_profile(&self, room_id: &[u8], user_id: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT INTO profile (room_id, user_id, data)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (room_id, user_id) DO UPDATE SET data = ?3",
        )?
        .execute((room_id, user_id, data))?;
        Ok(())
    }

    fn set_global_account_data(&self, event_type: &[u8], data: &[u8]) -> rusqlite::Result<()> {
        self.prepare_cached(
            "INSERT INTO global_account_data (event_type, data)
             VALUES (?1, ?2)
             ON CONFLICT (event_type) DO UPDATE SET data = ?2",
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
            "INSERT INTO room_account_data (room_id, event_type, data)
             VALUES (?1, ?2, ?3)
             ON CONFLICT (room_id, event_type) DO UPDATE SET data = ?3",
        )?
        .execute((room_id, event_type, data))?;
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

    async fn set_kv_blob(&self, key: Key, value: Vec<u8>) -> Result<()>;

    async fn delete_kv_blob(&self, key: Key) -> Result<()> {
        self.execute("DELETE FROM kv_blob WHERE key = ?", (key,)).await?;
        Ok(())
    }

    async fn get_room_infos(&self, stripped: bool) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare("SELECT data FROM room_info WHERE stripped = ?", move |mut stmt| {
                stmt.query_map((stripped,), |row| row.get(0))?.collect()
            })
            .await?)
    }

    async fn get_state_event(
        &self,
        room_id: Key,
        event_type: Key,
        state_key: Key,
    ) -> Result<Option<(bool, Vec<u8>)>> {
        Ok(self
            .query_row(
                "SELECT stripped, data FROM state_event
                 WHERE room_id = ? AND event_type = ? AND state_key = ?",
                (room_id, event_type, state_key),
                |row| Ok((row.get(1)?, row.get(0)?)),
            )
            .await
            .optional()?)
    }

    async fn get_state_events(&self, room_id: Key, event_type: Key) -> Result<Vec<Vec<u8>>> {
        Ok(self
            .prepare(
                "SELECT data FROM state_event WHERE room_id = ? AND event_type = ?",
                |mut stmt| stmt.query((room_id, event_type))?.mapped(|row| row.get(0)).collect(),
            )
            .await?)
    }

    async fn get_profile(&self, room_id: Key, user_id: Key) -> Result<Option<Vec<u8>>> {
        Ok(self
            .query_row(
                "SELECT data FROM profile WHERE room_id = ? AND user_id = ?",
                (room_id, user_id),
                |row| row.get(0),
            )
            .await
            .optional()?)
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
                    session,
                    account_data,
                    presence,
                    members,
                    profiles,
                    state,
                    room_account_data,
                    room_infos,
                    receipts,
                    redactions,
                    stripped_state,
                    stripped_members,
                    stripped_room_infos,
                    ambiguity_maps,
                    notifications,
                } = changes;

                if let Some(sync_token) = sync_token {
                    let key = this.encode_state_store_data_key(StateStoreDataKey::SyncToken);
                    let value = this.serialize_value(&sync_token)?;
                    txn.set_kv_blob(&key, &value)?;
                }

                // Never read, should this be persisted?
                if let Some(session) = session {
                    txn.set_kv("session", &this.serialize_value(&session)?)?;
                }

                for (event_type, event) in account_data {
                    let event_type = this.encode_key("global_account_data", event_type.to_string());
                    let data = this.serialize_json(&event)?;
                    txn.set_global_account_data(&event_type, &data)?;
                }

                for (room_id, events) in room_account_data {
                    let room_id = this.encode_key("room_account_data", room_id);
                    for (event_type, event) in events {
                        let event_type =
                            this.encode_key("room_account_data", event_type.to_string());
                        let data = this.serialize_json(&event)?;
                        txn.set_room_account_data(&room_id, &event_type, &data)?;
                    }
                }

                for (user_id, event) in presence {
                    let key = this.encode_presence_key(&user_id);
                    let value = this.serialize_json(&event)?;
                    txn.set_kv_blob(&key, &value)?;
                }

                for _ in members {}
                for _ in stripped_members {}

                for (room_id, profiles) in profiles {
                    let room_id = this.encode_key("profile", room_id);
                    for (user_id, profile) in profiles {
                        let user_id = this.encode_key("profile", user_id);
                        let data = this.serialize_json(&profile)?;
                        txn.set_profile(&room_id, &user_id, &data)?;
                    }
                }

                for (_, _) in state {}
                for (_, _) in stripped_state {}

                for (room_id, room_info) in chain(room_infos, stripped_room_infos) {
                    let room_id = this.encode_key("room_info", room_id);
                    let data = this.serialize_json(&room_info)?;
                    txn.set_room_info(&room_id, room_info.state() == RoomState::Invited, &data)?;
                }

                for _ in receipts {}

                for _ in redactions {}

                for _ in ambiguity_maps {}

                for _ in notifications {}

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

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        let room_id = self.encode_key("state", room_id.as_str());
        let event_type = self.encode_key("state", event_type.to_string());
        let state_key = self.encode_key("state", state_key);
        self.acquire()
            .await?
            .get_state_event(room_id, event_type, state_key)
            .await?
            .filter(|(stripped, _)| !stripped)
            .map(|(_, data)| self.deserialize_json(&data))
            .transpose()
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let room_id = self.encode_key("state", room_id.as_str());
        let event_type = self.encode_key("state", event_type.to_string());
        self.acquire()
            .await?
            .get_state_events(room_id, event_type)
            .await?
            .iter()
            .map(|data| self.deserialize_json(data))
            .collect()
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<matrix_sdk_base::MinimalRoomMemberEvent>> {
        let room_id = self.encode_key("profile", room_id.as_str());
        let user_id = self.encode_key("profile", user_id.as_str());
        self.acquire()
            .await?
            .get_profile(room_id, user_id)
            .await?
            .map(|data| self.deserialize_json(&data))
            .transpose()
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<RawMemberEvent>> {
        let room_id = self.encode_key("state", room_id.as_str());
        let event_type = self.encode_key("state", StateEventType::RoomMember.to_string());
        let state_key = self.encode_key("state", state_key);

        self.acquire()
            .await?
            .get_state_event(room_id, event_type, state_key)
            .await?
            .map(|(stripped, data)| {
                Ok(if stripped {
                    RawMemberEvent::Stripped(self.deserialize_json(&data)?)
                } else {
                    RawMemberEvent::Sync(self.deserialize_json(&data)?)
                })
            })
            .transpose()
    }

    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        todo!()
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        todo!()
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        todo!()
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.acquire()
            .await?
            .get_room_infos(false)
            .await?
            .into_iter()
            .map(|data| self.deserialize_json(&data))
            .collect()
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        self.acquire()
            .await?
            .get_room_infos(true)
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
        todo!()
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        let event_type = self.encode_key("state", event_type.to_string());
        self.acquire()
            .await?
            .get_global_account_data(event_type)
            .await?
            .map(|value| self.deserialize_value(&value))
            .transpose()
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        let room_id = self.encode_key("state", room_id.as_str());
        let event_type = self.encode_key("state", event_type.to_string());
        self.acquire()
            .await?
            .get_room_account_data(room_id, event_type)
            .await?
            .map(|value| self.deserialize_value(&value))
            .transpose()
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(ruma::OwnedEventId, Receipt)>> {
        todo!()
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        todo!()
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

    async fn add_media_content(&self, _request: &MediaRequest, _content: Vec<u8>) -> Result<()> {
        // TODO
        Ok(())
    }

    async fn get_media_content(&self, _request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        // TODO
        Ok(None)
    }

    async fn remove_media_content(&self, _request: &MediaRequest) -> Result<()> {
        // TODO
        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &ruma::MxcUri) -> Result<()> {
        // TODO
        Ok(())
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        todo!()
    }
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
