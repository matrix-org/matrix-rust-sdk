// Copyright 2021 The Matrix.org Foundation C.I.C.
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

use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::Arc,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use futures_core::stream::Stream;
use futures_util::stream::{self, StreamExt, TryStreamExt};
use matrix_sdk_base::{
    deserialized_responses::RawMemberEvent,
    media::{MediaRequest, UniqueKey},
    store::{Result as StoreResult, StateChanges, StateStore, StoreError},
    MinimalStateEvent, RoomInfo, StateStoreDataKey, StateStoreDataValue,
};
use matrix_sdk_store_encryption::{Error as KeyEncryptionError, StoreCipher};
use ruma::{
    canonical_json::redact,
    events::{
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::member::{MembershipState, RoomMemberEventContent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncStateEvent,
        GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    serde::Raw,
    CanonicalJsonObject, EventId, IdParseError, MxcUri, OwnedEventId, OwnedUserId, RoomId,
    RoomVersionId, UserId,
};
use serde::{de::DeserializeOwned, Serialize};
use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, Transactional, Tree,
};
use tokio::task::spawn_blocking;
use tracing::{debug, info, warn};

mod migrations;

pub use self::migrations::MigrationConflictStrategy;
#[cfg(feature = "crypto-store")]
use super::OpenStoreError;
use crate::encode_key::{EncodeKey, EncodeUnchecked};
#[cfg(feature = "crypto-store")]
pub use crate::SledCryptoStore;

#[derive(Debug, thiserror::Error)]
pub enum SledStoreError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] KeyEncryptionError),
    #[error(transparent)]
    StoreError(#[from] StoreError),
    #[error(transparent)]
    TransactionError(#[from] sled::Error),
    #[error(transparent)]
    Identifier(#[from] IdParseError),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    FsExtra(#[from] fs_extra::error::Error),
    #[error("Can't migrate {path} from {old_version} to {new_version} without deleting data. See MigrationConflictStrategy for ways to configure.")]
    MigrationConflict { path: PathBuf, old_version: usize, new_version: usize },
}

impl From<TransactionError<SledStoreError>> for SledStoreError {
    fn from(e: TransactionError<SledStoreError>) -> Self {
        match e {
            TransactionError::Abort(e) => e,
            TransactionError::Storage(e) => SledStoreError::TransactionError(e),
        }
    }
}

impl From<SledStoreError> for StoreError {
    fn from(err: SledStoreError) -> StoreError {
        match err {
            SledStoreError::Json(e) => StoreError::Json(e),
            SledStoreError::Identifier(e) => StoreError::Identifier(e),
            SledStoreError::Encryption(e) => StoreError::Encryption(e),
            SledStoreError::StoreError(e) => e,
            _ => StoreError::backend(err),
        }
    }
}

mod keys {
    // Static keys
    pub const SESSION: &str = "session";

    // Stores
    pub const ACCOUNT_DATA: &str = "account-data";
    pub const CUSTOM: &str = "custom";
    pub const DISPLAY_NAME: &str = "display-name";
    pub const INVITED_USER_ID: &str = "invited-user-id";
    pub const JOINED_USER_ID: &str = "joined-user-id";
    pub const MEDIA: &str = "media";
    pub const MEMBER: &str = "member";
    pub const PRESENCE: &str = "presence";
    pub const PROFILE: &str = "profile";
    pub const ROOM_ACCOUNT_DATA: &str = "room-account-data";
    pub const ROOM_EVENT_RECEIPT: &str = "room-event-receipt";
    pub const ROOM_INFO: &str = "room-info";
    pub const ROOM_STATE: &str = "room-state";
    pub const ROOM_USER_RECEIPT: &str = "room-user-receipt";
    pub const ROOM: &str = "room";
    pub const STRIPPED_INVITED_USER_ID: &str = "stripped-invited-user-id";
    pub const STRIPPED_JOINED_USER_ID: &str = "stripped-joined-user-id";
    pub const STRIPPED_ROOM_INFO: &str = "stripped-room-info";
    pub const STRIPPED_ROOM_MEMBER: &str = "stripped-room-member";
    pub const STRIPPED_ROOM_STATE: &str = "stripped-room-state";
    pub const KV: &str = "kv";
}

type Result<A, E = SledStoreError> = std::result::Result<A, E>;

#[derive(Debug, Clone)]
enum DbOrPath {
    Db(Db),
    Path(PathBuf),
}

/// Builder for [`SledStateStore`].
#[derive(Debug)]
pub struct SledStateStoreBuilder {
    db_or_path: Option<DbOrPath>,
    passphrase: Option<String>,
    migration_conflict_strategy: MigrationConflictStrategy,
}

impl SledStateStoreBuilder {
    fn new() -> Self {
        Self {
            db_or_path: None,
            passphrase: None,
            migration_conflict_strategy: MigrationConflictStrategy::BackupAndDrop,
        }
    }

    /// Path to the sled store files, created if not it doesn't exist yet.
    ///
    /// Mutually exclusive with [`db`][Self::db], whichever is called last wins.
    pub fn path(mut self, path: PathBuf) -> Self {
        self.db_or_path = Some(DbOrPath::Path(path));
        self
    }

    /// Use the given [`sled::Db`].
    ///
    /// Mutually exclusive with [`path`][Self::path], whichever is called last
    /// wins.
    pub fn db(mut self, db: Db) -> Self {
        self.db_or_path = Some(DbOrPath::Db(db));
        self
    }

    /// Set the password the sled store is encrypted with (if any).
    pub fn passphrase(mut self, value: String) -> Self {
        self.passphrase = Some(value);
        self
    }

    /// Set the strategy to use when a merge conflict is found.
    ///
    /// See [`MigrationConflictStrategy`] for details.
    pub fn migration_conflict_strategy(mut self, value: MigrationConflictStrategy) -> Self {
        self.migration_conflict_strategy = value;
        self
    }

    /// Create a [`SledStateStore`] with the options set on this builder.
    ///
    /// # Errors
    ///
    /// This method can fail for two general reasons:
    ///
    /// * Invalid path: The [`sled::Db`] could not be opened at the supplied
    ///   path.
    /// * Migration error: The migration to a newer version of the schema
    ///   failed, see `SledStoreError::MigrationConflict`.
    pub fn build(self) -> Result<SledStateStore> {
        let (db, path) = match &self.db_or_path {
            None => {
                let db = Config::new().temporary(true).open().map_err(StoreError::backend)?;
                (db, None)
            }
            Some(DbOrPath::Db(db)) => (db.clone(), None),
            Some(DbOrPath::Path(path)) => {
                let path = path.join("matrix-sdk-state");
                let db = Config::new().path(&path).open().map_err(StoreError::backend)?;
                (db, Some(path))
            }
        };

        let store_cipher = if let Some(passphrase) = &self.passphrase {
            if let Some(inner) = db.get("store_cipher".encode())? {
                Some(StoreCipher::import(passphrase, &inner)?.into())
            } else {
                let cipher = StoreCipher::new()?;
                #[cfg(not(test))]
                let export = cipher.export(passphrase)?;
                #[cfg(test)]
                let export = cipher._insecure_export_fast_for_testing(passphrase)?;
                db.insert("store_cipher".encode(), export)?;
                Some(cipher.into())
            }
        } else {
            None
        };

        let mut store = SledStateStore::open_helper(db, path, store_cipher)?;

        let migration_res = store.upgrade();
        if let Err(SledStoreError::MigrationConflict { path, .. }) = &migration_res {
            // how  are supposed to react about this?
            match self.migration_conflict_strategy {
                MigrationConflictStrategy::BackupAndDrop => {
                    let mut new_path = path.clone();
                    new_path.set_extension(format!(
                        "{}.backup",
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .expect("time doesn't go backwards")
                            .as_secs()
                    ));
                    fs_extra::dir::create_all(&new_path, false)?;
                    fs_extra::dir::copy(path, new_path, &fs_extra::dir::CopyOptions::new())?;
                    store.drop_v1_tables()?;
                    return self.build();
                }
                MigrationConflictStrategy::Drop => {
                    store.drop_v1_tables()?;
                    return self.build();
                }
                MigrationConflictStrategy::Raise => migration_res?,
            }
        } else {
            migration_res?;
        }

        Ok(store)
    }

    // testing only
    #[cfg(test)]
    fn build_encrypted() -> StoreResult<SledStateStore> {
        let db = Config::new().temporary(true).open().map_err(StoreError::backend)?;

        SledStateStore::open_helper(
            db,
            None,
            Some(StoreCipher::new().expect("can't create store cipher").into()),
        )
        .map_err(|e| e.into())
    }
}

#[derive(Clone)]
pub struct SledStateStore {
    path: Option<PathBuf>,
    pub(crate) inner: Db,
    store_cipher: Option<Arc<StoreCipher>>,
    kv: Tree,
    account_data: Tree,
    members: Tree,
    profiles: Tree,
    display_names: Tree,
    joined_user_ids: Tree,
    invited_user_ids: Tree,
    room_info: Tree,
    room_state: Tree,
    room_account_data: Tree,
    stripped_joined_user_ids: Tree,
    stripped_invited_user_ids: Tree,
    stripped_room_infos: Tree,
    stripped_room_state: Tree,
    stripped_members: Tree,
    presence: Tree,
    room_user_receipts: Tree,
    room_event_receipts: Tree,
    media: Tree,
    custom: Tree,
}

impl std::fmt::Debug for SledStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.path {
            f.debug_struct("SledStateStore").field("path", &path).finish()
        } else {
            f.debug_struct("SledStateStore").field("path", &"memory store").finish()
        }
    }
}

impl SledStateStore {
    fn open_helper(
        db: Db,
        path: Option<PathBuf>,
        store_cipher: Option<Arc<StoreCipher>>,
    ) -> Result<Self> {
        let kv = db.open_tree(keys::KV)?;
        let account_data = db.open_tree(keys::ACCOUNT_DATA)?;

        let members = db.open_tree(keys::MEMBER)?;
        let profiles = db.open_tree(keys::PROFILE)?;
        let display_names = db.open_tree(keys::DISPLAY_NAME)?;
        let joined_user_ids = db.open_tree(keys::JOINED_USER_ID)?;
        let invited_user_ids = db.open_tree(keys::INVITED_USER_ID)?;

        let room_state = db.open_tree(keys::ROOM_STATE)?;
        let room_info = db.open_tree(keys::ROOM_INFO)?;
        let presence = db.open_tree(keys::PRESENCE)?;
        let room_account_data = db.open_tree(keys::ROOM_ACCOUNT_DATA)?;

        let stripped_joined_user_ids = db.open_tree(keys::STRIPPED_JOINED_USER_ID)?;
        let stripped_invited_user_ids = db.open_tree(keys::STRIPPED_INVITED_USER_ID)?;
        let stripped_room_infos = db.open_tree(keys::STRIPPED_ROOM_INFO)?;
        let stripped_members = db.open_tree(keys::STRIPPED_ROOM_MEMBER)?;
        let stripped_room_state = db.open_tree(keys::STRIPPED_ROOM_STATE)?;

        let room_user_receipts = db.open_tree(keys::ROOM_USER_RECEIPT)?;
        let room_event_receipts = db.open_tree(keys::ROOM_EVENT_RECEIPT)?;

        let media = db.open_tree(keys::MEDIA)?;

        let custom = db.open_tree(keys::CUSTOM)?;

        Ok(Self {
            path,
            inner: db,
            store_cipher,
            kv,
            account_data,
            members,
            profiles,
            display_names,
            joined_user_ids,
            invited_user_ids,
            room_account_data,
            presence,
            room_state,
            room_info,
            stripped_joined_user_ids,
            stripped_invited_user_ids,
            stripped_room_infos,
            stripped_members,
            stripped_room_state,
            room_user_receipts,
            room_event_receipts,
            media,
            custom,
        })
    }

    /// Create a [`SledStateStoreBuilder`] with default parameters.
    pub fn builder() -> SledStateStoreBuilder {
        SledStateStoreBuilder::new()
    }

    /// Open a `SledCryptoStore` that uses the same database as this store.
    ///
    /// The given passphrase will be used to encrypt private data.
    #[cfg(feature = "crypto-store")]
    pub async fn open_crypto_store(&self) -> Result<SledCryptoStore, OpenStoreError> {
        let db = self.inner.clone();
        let store_cipher = self.store_cipher.clone();
        SledCryptoStore::open_helper(db, None, store_cipher).await
    }

    fn serialize_value(&self, event: &impl Serialize) -> Result<Vec<u8>, SledStoreError> {
        if let Some(key) = &self.store_cipher {
            Ok(key.encrypt_value(event)?)
        } else {
            Ok(serde_json::to_vec(event)?)
        }
    }

    fn deserialize_value<T: DeserializeOwned>(&self, event: &[u8]) -> Result<T, SledStoreError> {
        if let Some(key) = &self.store_cipher {
            Ok(key.decrypt_value(event)?)
        } else {
            Ok(serde_json::from_slice(event)?)
        }
    }

    fn encode_key<T: EncodeKey>(&self, table_name: &str, key: T) -> Vec<u8> {
        if let Some(store_cipher) = &self.store_cipher {
            key.encode_secure(table_name, store_cipher).to_vec()
        } else {
            key.encode()
        }
    }

    fn encode_kv_data_key(&self, key: StateStoreDataKey<'_>) -> Vec<u8> {
        match key {
            StateStoreDataKey::SyncToken => key.encoding_key().encode(),
            StateStoreDataKey::Filter(filter_name) => {
                self.encode_key(keys::SESSION, (key.encoding_key(), filter_name))
            }
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                self.encode_key(keys::SESSION, (key.encoding_key(), user_id))
            }
        }
    }

    async fn get_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<Option<StateStoreDataValue>> {
        let encoded_key = self.encode_kv_data_key(key);

        let value =
            self.kv.get(encoded_key)?.map(|e| self.deserialize_value::<String>(&e)).transpose()?;

        let value = match key {
            StateStoreDataKey::SyncToken => value.map(StateStoreDataValue::SyncToken),
            StateStoreDataKey::Filter(_) => value.map(StateStoreDataValue::Filter),
            StateStoreDataKey::UserAvatarUrl(_) => value.map(StateStoreDataValue::UserAvatarUrl),
        };

        Ok(value)
    }

    async fn set_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
        value: StateStoreDataValue,
    ) -> Result<()> {
        let encoded_key = self.encode_kv_data_key(key);

        let value = match key {
            StateStoreDataKey::SyncToken => {
                value.into_sync_token().expect("Session data not a sync token")
            }
            StateStoreDataKey::Filter(_) => value.into_filter().expect("Session data not a filter"),
            StateStoreDataKey::UserAvatarUrl(_) => {
                value.into_user_avatar_url().expect("Session data not an user avatar url")
            }
        };

        self.kv.insert(encoded_key, self.serialize_value(&value)?)?;

        Ok(())
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<()> {
        let encoded_key = self.encode_kv_data_key(key);

        self.kv.remove(encoded_key)?;

        Ok(())
    }

    pub async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let now = Instant::now();

        // room state & memberships
        let ret: Result<(), TransactionError<SledStoreError>> = (
            &self.members,
            &self.profiles,
            &self.display_names,
            &self.joined_user_ids,
            &self.invited_user_ids,
            &self.room_info,
            &self.room_state,
            &self.room_account_data,
            &self.stripped_joined_user_ids,
            &self.stripped_invited_user_ids,
            &self.stripped_room_infos,
            &self.stripped_members,
            &self.stripped_room_state,
        )
            .transaction(
                |(
                    members,
                    profiles,
                    display_names,
                    joined,
                    invited,
                    rooms,
                    state,
                    room_account_data,
                    stripped_joined,
                    stripped_invited,
                    stripped_rooms,
                    stripped_members,
                    stripped_state,
                )| {
                    for (room, raw_events) in &changes.members {
                        let profile_changes = changes.profiles.get(room);

                        for raw_event in raw_events.values() {
                            let event = match raw_event.deserialize() {
                                Ok(ev) => ev,
                                Err(e) => {
                                    let event_id: Option<String> =
                                        raw_event.get_field("event_id").ok().flatten();
                                    debug!(event_id, "Failed to deserialize member event: {e}");
                                    continue;
                                }
                            };

                            let key = (room, event.state_key());

                            stripped_joined
                                .remove(self.encode_key(keys::STRIPPED_JOINED_USER_ID, key))?;
                            stripped_invited
                                .remove(self.encode_key(keys::STRIPPED_INVITED_USER_ID, key))?;

                            match event.membership() {
                                MembershipState::Join => {
                                    joined.insert(
                                        self.encode_key(keys::JOINED_USER_ID, key),
                                        self.serialize_value(event.state_key())
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )?;
                                    invited.remove(self.encode_key(keys::INVITED_USER_ID, key))?;
                                }
                                MembershipState::Invite => {
                                    invited.insert(
                                        self.encode_key(keys::INVITED_USER_ID, key),
                                        self.serialize_value(event.state_key())
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )?;
                                    joined.remove(self.encode_key(keys::JOINED_USER_ID, key))?;
                                }
                                _ => {
                                    joined.remove(self.encode_key(keys::JOINED_USER_ID, key))?;
                                    invited.remove(self.encode_key(keys::INVITED_USER_ID, key))?;
                                }
                            }

                            members.insert(
                                self.encode_key(keys::MEMBER, key),
                                self.serialize_value(&raw_event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                            stripped_members
                                .remove(self.encode_key(keys::STRIPPED_ROOM_MEMBER, key))?;

                            if let Some(profile) =
                                profile_changes.and_then(|p| p.get(event.state_key()))
                            {
                                profiles.insert(
                                    self.encode_key(keys::PROFILE, key),
                                    self.serialize_value(&profile)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    for (room_id, ambiguity_maps) in &changes.ambiguity_maps {
                        for (display_name, map) in ambiguity_maps {
                            display_names.insert(
                                self.encode_key(keys::DISPLAY_NAME, (room_id, display_name)),
                                self.serialize_value(&map)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, events) in &changes.room_account_data {
                        for (event_type, event) in events {
                            room_account_data.insert(
                                self.encode_key(keys::ROOM_ACCOUNT_DATA, (room, event_type)),
                                self.serialize_value(&event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.state {
                        for (event_type, events) in event_types {
                            for (state_key, event) in events {
                                state.insert(
                                    self.encode_key(
                                        keys::ROOM_STATE,
                                        (room, event_type, state_key),
                                    ),
                                    self.serialize_value(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                                stripped_state.remove(self.encode_key(
                                    keys::STRIPPED_ROOM_STATE,
                                    (room, event_type, state_key),
                                ))?;
                            }
                        }
                    }

                    for (room_id, room_info) in &changes.room_infos {
                        rooms.insert(
                            self.encode_key(keys::ROOM, room_id),
                            self.serialize_value(room_info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                        stripped_rooms
                            .remove(self.encode_key(keys::STRIPPED_ROOM_INFO, room_id))?;
                    }

                    for (room_id, info) in &changes.stripped_room_infos {
                        stripped_rooms.insert(
                            self.encode_key(keys::STRIPPED_ROOM_INFO, room_id),
                            self.serialize_value(&info)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                        rooms.remove(self.encode_key(keys::ROOM, room_id))?;
                    }

                    for (room, raw_events) in &changes.stripped_members {
                        for raw_event in raw_events.values() {
                            let event = match raw_event.deserialize() {
                                Ok(ev) => ev,
                                Err(e) => {
                                    let event_id: Option<String> =
                                        raw_event.get_field("event_id").ok().flatten();
                                    debug!(
                                        event_id,
                                        "Failed to deserialize stripped member event: {e}"
                                    );
                                    continue;
                                }
                            };

                            let key = (room, &event.state_key);

                            match event.content.membership {
                                MembershipState::Join => {
                                    stripped_joined.insert(
                                        self.encode_key(keys::STRIPPED_JOINED_USER_ID, key),
                                        self.serialize_value(&event.state_key)
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )?;
                                    stripped_invited.remove(
                                        self.encode_key(keys::STRIPPED_INVITED_USER_ID, key),
                                    )?;
                                }
                                MembershipState::Invite => {
                                    stripped_invited.insert(
                                        self.encode_key(keys::STRIPPED_INVITED_USER_ID, key),
                                        self.serialize_value(&event.state_key)
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )?;
                                    stripped_joined.remove(
                                        self.encode_key(keys::STRIPPED_JOINED_USER_ID, key),
                                    )?;
                                }
                                _ => {
                                    stripped_joined.remove(
                                        self.encode_key(keys::STRIPPED_JOINED_USER_ID, key),
                                    )?;
                                    stripped_invited.remove(
                                        self.encode_key(keys::STRIPPED_INVITED_USER_ID, key),
                                    )?;
                                }
                            }
                            stripped_members.insert(
                                self.encode_key(keys::STRIPPED_ROOM_MEMBER, key),
                                self.serialize_value(&raw_event)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    for (room, event_types) in &changes.stripped_state {
                        for (event_type, events) in event_types {
                            for (state_key, event) in events {
                                stripped_state.insert(
                                    self.encode_key(
                                        keys::STRIPPED_ROOM_STATE,
                                        (room, event_type.to_string(), state_key),
                                    ),
                                    self.serialize_value(&event)
                                        .map_err(ConflictableTransactionError::Abort)?,
                                )?;
                            }
                        }
                    }

                    Ok(())
                },
            );

        ret?;

        if !changes.redactions.is_empty() {
            let mut redactions_found = false;
            let mut redactions_batch = sled::Batch::default();

            let make_room_version = |room_id| {
                self.room_info
                    .get(self.encode_key(keys::ROOM, room_id))
                    .ok()
                    .flatten()
                    .map(|r| self.deserialize_value::<RoomInfo>(&r))
                    .transpose()
                    .ok()
                    .flatten()
                    .and_then(|info| info.room_version().cloned())
                    .unwrap_or_else(|| {
                        warn!(?room_id, "Unable to find the room version, assume version 9");
                        RoomVersionId::V9
                    })
            };

            for (room_id, redactions) in &changes.redactions {
                let key_prefix = self.encode_key(keys::ROOM_STATE, room_id);
                let mut room_version = None;

                // iterate through all saved state events and check whether they are among the
                // redacted, if so apply redaction and save that to the batch
                // applied all at once after.
                for (key, evt) in self.room_state.scan_prefix(key_prefix).filter_map(|r| r.ok()) {
                    let raw_evt = self.deserialize_value::<Raw<AnySyncStateEvent>>(&evt)?;
                    if let Ok(Some(event_id)) = raw_evt.get_field::<OwnedEventId>("event_id") {
                        if let Some(redaction) = redactions.get(&event_id) {
                            let redacted = redact(
                                raw_evt.deserialize_as::<CanonicalJsonObject>()?,
                                room_version.get_or_insert_with(|| make_room_version(room_id)),
                                Some(redaction.try_into()?),
                            )
                            .map_err(StoreError::Redaction)?;
                            redactions_found = true;
                            redactions_batch.insert(key, self.serialize_value(&redacted)?);
                        }
                    }
                }
            }

            if redactions_found {
                self.room_state.apply_batch(redactions_batch)?;
            }
        }

        let ret: Result<(), TransactionError<SledStoreError>> =
            (&self.room_user_receipts, &self.room_event_receipts, &self.presence).transaction(
                |(room_user_receipts, room_event_receipts, presence)| {
                    for (room, content) in &changes.receipts {
                        for (event_id, receipts) in &content.0 {
                            for (receipt_type, receipts) in receipts {
                                for (user_id, receipt) in receipts {
                                    // Add the receipt to the room user receipts
                                    let key = match receipt.thread.as_str() {
                                        Some(thread_id) => self.encode_key(
                                            keys::ROOM_USER_RECEIPT,
                                            (room, receipt_type, thread_id, user_id),
                                        ),
                                        None => self.encode_key(
                                            keys::ROOM_USER_RECEIPT,
                                            (room, receipt_type, user_id),
                                        ),
                                    };
                                    if let Some(old) = room_user_receipts.insert(
                                        key,
                                        self.serialize_value(&(event_id, receipt))
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )? {
                                        // Remove the old receipt from the room event receipts
                                        let (old_event, _): (OwnedEventId, Receipt) = self
                                            .deserialize_value(&old)
                                            .map_err(ConflictableTransactionError::Abort)?;
                                        let key = match receipt.thread.as_str() {
                                            Some(thread_id) => self.encode_key(
                                                keys::ROOM_EVENT_RECEIPT,
                                                (room, receipt_type, thread_id, old_event, user_id),
                                            ),
                                            None => self.encode_key(
                                                keys::ROOM_EVENT_RECEIPT,
                                                (room, receipt_type, old_event, user_id),
                                            ),
                                        };
                                        room_event_receipts.remove(key)?;
                                    }

                                    // Add the receipt to the room event receipts
                                    let key = match receipt.thread.as_str() {
                                        Some(thread_id) => self.encode_key(
                                            keys::ROOM_EVENT_RECEIPT,
                                            (room, receipt_type, thread_id, event_id, user_id),
                                        ),
                                        None => self.encode_key(
                                            keys::ROOM_EVENT_RECEIPT,
                                            (room, receipt_type, event_id, user_id),
                                        ),
                                    };
                                    room_event_receipts.insert(
                                        key,
                                        self.serialize_value(&(user_id, receipt))
                                            .map_err(ConflictableTransactionError::Abort)?,
                                    )?;
                                }
                            }
                        }
                    }

                    for (sender, event) in &changes.presence {
                        presence.insert(
                            self.encode_key(keys::PRESENCE, sender),
                            self.serialize_value(&event)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    Ok(())
                },
            );

        ret?;

        // user state
        let ret: Result<(), TransactionError<SledStoreError>> = (&self.kv, &self.account_data)
            .transaction(|(kv, account_data)| {
                if let Some(s) = &changes.sync_token {
                    kv.insert(
                        self.encode_kv_data_key(StateStoreDataKey::SyncToken),
                        self.serialize_value(s).map_err(ConflictableTransactionError::Abort)?,
                    )?;
                }

                for (event_type, event) in &changes.account_data {
                    account_data.insert(
                        self.encode_key(keys::ACCOUNT_DATA, event_type),
                        self.serialize_value(&event)
                            .map_err(ConflictableTransactionError::Abort)?,
                    )?;
                }

                Ok(())
            });

        ret?;

        self.inner.flush_async().await?;

        info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        let db = self.clone();
        let key = self.encode_key(keys::PRESENCE, user_id);
        spawn_blocking(move || db.presence.get(key)?.map(|e| db.deserialize_value(&e)).transpose())
            .await?
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        let db = self.clone();
        let key = self.encode_key(keys::ROOM_STATE, (room_id, event_type.to_string(), state_key));
        spawn_blocking(move || {
            db.room_state.get(key)?.map(|e| db.deserialize_value(&e)).transpose()
        })
        .await?
    }

    pub async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let db = self.clone();
        let key = self.encode_key(keys::ROOM_STATE, (room_id, event_type.to_string()));
        spawn_blocking(move || {
            db.room_state
                .scan_prefix(key)
                .flat_map(|e| e.map(|(_, e)| db.deserialize_value(&e)))
                .collect::<Result<_, _>>()
        })
        .await?
    }

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalStateEvent<RoomMemberEventContent>>> {
        let db = self.clone();
        let key = self.encode_key(keys::PROFILE, (room_id, user_id));
        spawn_blocking(move || db.profiles.get(key)?.map(|p| db.deserialize_value(&p)).transpose())
            .await?
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<RawMemberEvent>> {
        let db = self.clone();
        let key = self.encode_key(keys::MEMBER, (room_id, state_key));
        let stripped_key = self.encode_key(keys::STRIPPED_ROOM_MEMBER, (room_id, state_key));
        spawn_blocking(move || {
            if let Some(e) = db
                .stripped_members
                .get(stripped_key)?
                .map(|v| db.deserialize_value(&v))
                .transpose()?
            {
                Ok(Some(RawMemberEvent::Stripped(e)))
            } else if let Some(e) =
                db.members.get(key)?.map(|v| db.deserialize_value(&v)).transpose()?
            {
                Ok(Some(RawMemberEvent::Sync(e)))
            } else {
                Ok(None)
            }
        })
        .await?
    }

    pub async fn get_user_ids_stream(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        Ok(self
            .get_joined_user_ids(room_id)
            .await?
            .chain(self.get_invited_user_ids(room_id).await?))
    }
    pub async fn get_stripped_user_ids_stream(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        Ok(self
            .get_stripped_joined_user_ids(room_id)
            .await?
            .chain(self.get_stripped_invited_user_ids(room_id).await?))
    }

    pub async fn get_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        let db = self.clone();
        let key = self.encode_key(keys::INVITED_USER_ID, room_id);
        spawn_blocking(move || {
            stream::iter(db.invited_user_ids.scan_prefix(key).map(move |u| {
                db.deserialize_value(&u.map_err(StoreError::backend)?.1)
                    .map_err(StoreError::backend)
            }))
        })
        .await
        .map_err(StoreError::backend)
    }

    pub async fn get_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        let db = self.clone();
        let key = self.encode_key(keys::JOINED_USER_ID, room_id);
        spawn_blocking(move || {
            stream::iter(db.joined_user_ids.scan_prefix(key).map(move |u| {
                db.deserialize_value(&u.map_err(StoreError::backend)?.1)
                    .map_err(StoreError::backend)
            }))
        })
        .await
        .map_err(StoreError::backend)
    }

    pub async fn get_stripped_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        let db = self.clone();
        let key = self.encode_key(keys::STRIPPED_INVITED_USER_ID, room_id);
        spawn_blocking(move || {
            stream::iter(db.stripped_invited_user_ids.scan_prefix(key).map(move |u| {
                db.deserialize_value(&u.map_err(StoreError::backend)?.1)
                    .map_err(StoreError::backend)
            }))
        })
        .await
        .map_err(StoreError::backend)
    }

    pub async fn get_stripped_joined_user_ids(
        &self,
        room_id: &RoomId,
    ) -> StoreResult<impl Stream<Item = StoreResult<OwnedUserId>>> {
        let db = self.clone();
        let key = self.encode_key(keys::STRIPPED_JOINED_USER_ID, room_id);
        spawn_blocking(move || {
            stream::iter(db.stripped_joined_user_ids.scan_prefix(key).map(move |u| {
                db.deserialize_value(&u.map_err(StoreError::backend)?.1)
                    .map_err(StoreError::backend)
            }))
        })
        .await
        .map_err(StoreError::backend)
    }

    pub async fn get_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let db = self.clone();
        spawn_blocking(move || {
            stream::iter(db.room_info.iter().map(move |r| db.deserialize_value(&r?.1)))
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_stripped_room_infos(&self) -> Result<impl Stream<Item = Result<RoomInfo>>> {
        let db = self.clone();
        spawn_blocking(move || {
            stream::iter(db.stripped_room_infos.iter().map(move |r| db.deserialize_value(&r?.1)))
        })
        .await
        .map_err(Into::into)
    }

    pub async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>> {
        let db = self.clone();
        let key = self.encode_key(keys::DISPLAY_NAME, (room_id, display_name));
        spawn_blocking(move || {
            Ok(db
                .display_names
                .get(key)?
                .map(|m| db.deserialize_value(&m))
                .transpose()?
                .unwrap_or_default())
        })
        .await?
    }

    pub async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        let db = self.clone();
        let key = self.encode_key(keys::ACCOUNT_DATA, event_type);
        spawn_blocking(move || {
            db.account_data.get(key)?.map(|m| db.deserialize_value(&m)).transpose()
        })
        .await?
    }

    pub async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        let db = self.clone();
        let key = self.encode_key(keys::ROOM_ACCOUNT_DATA, (room_id, event_type));
        spawn_blocking(move || {
            db.room_account_data.get(key)?.map(|m| db.deserialize_value(&m)).transpose()
        })
        .await?
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        let db = self.clone();
        let key = match thread.as_str() {
            Some(thread_id) => self
                .encode_key(keys::ROOM_USER_RECEIPT, (room_id, receipt_type, thread_id, user_id)),
            None => self.encode_key(keys::ROOM_USER_RECEIPT, (room_id, receipt_type, user_id)),
        };
        spawn_blocking(move || {
            db.room_user_receipts.get(key)?.map(|m| db.deserialize_value(&m)).transpose()
        })
        .await?
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        let db = self.clone();
        let key = match thread.as_str() {
            Some(thread_id) => self
                .encode_key(keys::ROOM_EVENT_RECEIPT, (room_id, receipt_type, thread_id, event_id)),
            None => self.encode_key(keys::ROOM_EVENT_RECEIPT, (room_id, receipt_type, event_id)),
        };
        spawn_blocking(move || {
            db.room_event_receipts
                .scan_prefix(key)
                .values()
                .map(|u| {
                    let v = u.map_err(StoreError::backend)?;
                    db.deserialize_value(&v).map_err(StoreError::backend)
                })
                .collect()
        })
        .await
        .map_err(StoreError::backend)?
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        self.media.insert(
            self.encode_key(
                keys::MEDIA,
                (request.source.unique_key(), request.format.unique_key()),
            ),
            self.serialize_value(&data)?,
        )?;

        self.inner.flush_async().await?;

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let db = self.clone();
        let key = self
            .encode_key(keys::MEDIA, (request.source.unique_key(), request.format.unique_key()));

        spawn_blocking(move || {
            db.media.get(key)?.map(move |m| db.deserialize_value(&m)).transpose()
        })
        .await?
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let custom = self.custom.clone();
        let me = self.clone();
        let key = self.encode_key(keys::CUSTOM, EncodeUnchecked::from(key));
        spawn_blocking(move || custom.get(key)?.map(move |v| me.deserialize_value(&v)).transpose())
            .await?
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let key = self.encode_key(keys::CUSTOM, EncodeUnchecked::from(key));
        let me = self.clone();
        let ret = self
            .custom
            .insert(key, me.serialize_value(&value)?)?
            .map(|v| me.deserialize_value(&v))
            .transpose();
        self.inner.flush_async().await?;

        ret
    }

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let key = self.encode_key(keys::CUSTOM, EncodeUnchecked::from(key));
        let me = self.clone();
        let ret = self.custom.remove(key)?.map(|v| me.deserialize_value(&v)).transpose();
        self.inner.flush_async().await?;

        ret
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        self.media.remove(
            self.encode_key(
                keys::MEDIA,
                (request.source.unique_key(), request.format.unique_key()),
            ),
        )?;

        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let keys = self.media.scan_prefix(self.encode_key(keys::MEDIA, uri)).keys();

        let mut batch = sled::Batch::default();
        for key in keys {
            batch.remove(key?);
        }

        Ok(self.media.apply_batch(batch)?)
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        let mut members_batch = sled::Batch::default();
        for key in self.members.scan_prefix(self.encode_key(keys::MEMBER, room_id)).keys() {
            members_batch.remove(key?);
        }

        let mut stripped_members_batch = sled::Batch::default();
        for key in self
            .stripped_members
            .scan_prefix(self.encode_key(keys::STRIPPED_ROOM_MEMBER, room_id))
            .keys()
        {
            stripped_members_batch.remove(key?);
        }

        let mut profiles_batch = sled::Batch::default();
        for key in self.profiles.scan_prefix(self.encode_key(keys::PROFILE, room_id)).keys() {
            profiles_batch.remove(key?);
        }

        let mut display_names_batch = sled::Batch::default();
        for key in
            self.display_names.scan_prefix(self.encode_key(keys::DISPLAY_NAME, room_id)).keys()
        {
            display_names_batch.remove(key?);
        }

        let mut joined_user_ids_batch = sled::Batch::default();
        for key in
            self.joined_user_ids.scan_prefix(self.encode_key(keys::JOINED_USER_ID, room_id)).keys()
        {
            joined_user_ids_batch.remove(key?);
        }

        let mut stripped_joined_user_ids_batch = sled::Batch::default();
        for key in self
            .stripped_joined_user_ids
            .scan_prefix(self.encode_key(keys::STRIPPED_JOINED_USER_ID, room_id))
            .keys()
        {
            stripped_joined_user_ids_batch.remove(key?);
        }

        let mut invited_user_ids_batch = sled::Batch::default();
        for key in self
            .invited_user_ids
            .scan_prefix(self.encode_key(keys::INVITED_USER_ID, room_id))
            .keys()
        {
            invited_user_ids_batch.remove(key?);
        }

        let mut stripped_invited_user_ids_batch = sled::Batch::default();
        for key in self
            .stripped_invited_user_ids
            .scan_prefix(self.encode_key(keys::STRIPPED_INVITED_USER_ID, room_id))
            .keys()
        {
            stripped_invited_user_ids_batch.remove(key?);
        }

        let mut room_state_batch = sled::Batch::default();
        for key in self.room_state.scan_prefix(self.encode_key(keys::ROOM_STATE, room_id)).keys() {
            room_state_batch.remove(key?);
        }

        let mut stripped_room_state_batch = sled::Batch::default();
        for key in self
            .stripped_room_state
            .scan_prefix(self.encode_key(keys::STRIPPED_ROOM_STATE, room_id))
            .keys()
        {
            stripped_room_state_batch.remove(key?);
        }

        let mut room_account_data_batch = sled::Batch::default();
        for key in self
            .room_account_data
            .scan_prefix(self.encode_key(keys::ROOM_ACCOUNT_DATA, room_id))
            .keys()
        {
            room_account_data_batch.remove(key?);
        }

        let ret: Result<(), TransactionError<SledStoreError>> = (
            &self.members,
            &self.stripped_members,
            &self.profiles,
            &self.display_names,
            &self.joined_user_ids,
            &self.stripped_joined_user_ids,
            &self.invited_user_ids,
            &self.stripped_invited_user_ids,
            &self.room_info,
            &self.stripped_room_infos,
            &self.room_state,
            &self.stripped_room_state,
            &self.room_account_data,
        )
            .transaction(
                |(
                    members,
                    stripped_members,
                    profiles,
                    display_names,
                    joined,
                    stripped_joined,
                    invited,
                    stripped_invited,
                    rooms,
                    stripped_rooms,
                    state,
                    stripped_state,
                    room_account_data,
                )| {
                    rooms.remove(self.encode_key(keys::ROOM, room_id))?;
                    stripped_rooms.remove(self.encode_key(keys::STRIPPED_ROOM_INFO, room_id))?;

                    members.apply_batch(&members_batch)?;
                    stripped_members.apply_batch(&stripped_members_batch)?;
                    profiles.apply_batch(&profiles_batch)?;
                    display_names.apply_batch(&display_names_batch)?;
                    joined.apply_batch(&joined_user_ids_batch)?;
                    stripped_joined.apply_batch(&stripped_joined_user_ids_batch)?;
                    invited.apply_batch(&invited_user_ids_batch)?;
                    stripped_invited.apply_batch(&stripped_invited_user_ids_batch)?;
                    state.apply_batch(&room_state_batch)?;
                    stripped_state.apply_batch(&stripped_room_state_batch)?;
                    room_account_data.apply_batch(&room_account_data_batch)?;

                    Ok(())
                },
            );
        ret?;

        let mut room_user_receipts_batch = sled::Batch::default();
        for key in self
            .room_user_receipts
            .scan_prefix(self.encode_key(keys::ROOM_USER_RECEIPT, room_id))
            .keys()
        {
            room_user_receipts_batch.remove(key?);
        }

        let mut room_event_receipts_batch = sled::Batch::default();
        for key in self
            .room_event_receipts
            .scan_prefix(self.encode_key(keys::ROOM_EVENT_RECEIPT, room_id))
            .keys()
        {
            room_event_receipts_batch.remove(key?);
        }

        let ret: Result<(), TransactionError<SledStoreError>> =
            (&self.room_user_receipts, &self.room_event_receipts).transaction(
                |(room_user_receipts, room_event_receipts)| {
                    room_user_receipts.apply_batch(&room_user_receipts_batch)?;
                    room_event_receipts.apply_batch(&room_event_receipts_batch)?;
                    Ok(())
                },
            );
        ret?;

        self.inner.flush_async().await?;

        Ok(())
    }
}

#[async_trait]
impl StateStore for SledStateStore {
    type Error = StoreError;

    async fn get_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
    ) -> StoreResult<Option<StateStoreDataValue>> {
        self.get_kv_data(key).await.map_err(Into::into)
    }

    async fn set_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
        value: StateStoreDataValue,
    ) -> StoreResult<()> {
        self.set_kv_data(key, value).await.map_err(Into::into)
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> StoreResult<()> {
        self.remove_kv_data(key).await.map_err(Into::into)
    }

    async fn save_changes(&self, changes: &StateChanges) -> StoreResult<()> {
        self.save_changes(changes).await.map_err(Into::into)
    }

    async fn get_presence_event(
        &self,
        user_id: &UserId,
    ) -> StoreResult<Option<Raw<PresenceEvent>>> {
        self.get_presence_event(user_id).await.map_err(Into::into)
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> StoreResult<Option<Raw<AnySyncStateEvent>>> {
        self.get_state_event(room_id, event_type, state_key).await.map_err(Into::into)
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> StoreResult<Vec<Raw<AnySyncStateEvent>>> {
        self.get_state_events(room_id, event_type).await.map_err(Into::into)
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> StoreResult<Option<MinimalStateEvent<RoomMemberEventContent>>> {
        self.get_profile(room_id, user_id).await.map_err(Into::into)
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> StoreResult<Option<RawMemberEvent>> {
        self.get_member_event(room_id, state_key).await.map_err(Into::into)
    }

    async fn get_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        let v: Vec<OwnedUserId> =
            self.get_stripped_user_ids_stream(room_id).await?.try_collect().await?;
        if !v.is_empty() {
            return Ok(v);
        }
        self.get_user_ids_stream(room_id).await?.try_collect().await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        let v: Vec<OwnedUserId> =
            self.get_stripped_invited_user_ids(room_id).await?.try_collect().await?;
        if !v.is_empty() {
            return Ok(v);
        }
        self.get_invited_user_ids(room_id).await?.try_collect().await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> StoreResult<Vec<OwnedUserId>> {
        let v: Vec<OwnedUserId> =
            self.get_stripped_joined_user_ids(room_id).await?.try_collect().await?;
        if !v.is_empty() {
            return Ok(v);
        }
        self.get_joined_user_ids(room_id).await?.try_collect().await
    }

    async fn get_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        self.get_room_infos()
            .await
            .map_err::<StoreError, _>(Into::into)?
            .try_collect()
            .await
            .map_err::<StoreError, _>(Into::into)
    }

    async fn get_stripped_room_infos(&self) -> StoreResult<Vec<RoomInfo>> {
        self.get_stripped_room_infos()
            .await
            .map_err::<StoreError, _>(Into::into)?
            .try_collect()
            .await
            .map_err::<StoreError, _>(Into::into)
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> StoreResult<BTreeSet<OwnedUserId>> {
        self.get_users_with_display_name(room_id, display_name).await.map_err(Into::into)
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> StoreResult<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.get_account_data_event(event_type).await.map_err(Into::into)
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> StoreResult<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.get_room_account_data_event(room_id, event_type).await.map_err(Into::into)
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> StoreResult<Option<(OwnedEventId, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, thread, user_id)
            .await
            .map_err(Into::into)
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> StoreResult<Vec<(OwnedUserId, Receipt)>> {
        self.get_event_room_receipt_events(room_id, receipt_type, thread, event_id)
            .await
            .map_err(Into::into)
    }

    async fn get_custom_value(&self, key: &[u8]) -> StoreResult<Option<Vec<u8>>> {
        self.get_custom_value(key).await.map_err(Into::into)
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> StoreResult<Option<Vec<u8>>> {
        self.set_custom_value(key, value).await.map_err(Into::into)
    }

    async fn remove_custom_value(&self, key: &[u8]) -> StoreResult<Option<Vec<u8>>> {
        self.remove_custom_value(key).await.map_err(Into::into)
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> StoreResult<()> {
        self.add_media_content(request, data).await.map_err(Into::into)
    }

    async fn get_media_content(&self, request: &MediaRequest) -> StoreResult<Option<Vec<u8>>> {
        self.get_media_content(request).await.map_err(Into::into)
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> StoreResult<()> {
        self.remove_media_content(request).await.map_err(Into::into)
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> StoreResult<()> {
        self.remove_media_content_for_uri(uri).await.map_err(Into::into)
    }

    async fn remove_room(&self, room_id: &RoomId) -> StoreResult<()> {
        self.remove_room(room_id).await.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_base::statestore_integration_tests;

    use super::{SledStateStore, StateStore, StoreResult};

    async fn get_store() -> StoreResult<impl StateStore> {
        SledStateStore::builder().build().map_err(Into::into)
    }

    statestore_integration_tests!(with_media_tests);
}

#[cfg(test)]
mod encrypted_tests {
    use matrix_sdk_base::statestore_integration_tests;

    use super::{SledStateStoreBuilder, StateStore, StoreResult};

    async fn get_store() -> StoreResult<impl StateStore> {
        SledStateStoreBuilder::build_encrypted().map_err(Into::into)
    }

    statestore_integration_tests!(with_media_tests);
}
