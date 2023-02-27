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
    collections::{BTreeSet, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::anyhow;
use async_trait::async_trait;
use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::prelude::*;
use js_sys::Date as JsDate;
use matrix_sdk_base::{
    deserialized_responses::RawMemberEvent,
    media::{MediaRequest, UniqueKey},
    store::{StateChanges, StateStore, StoreError},
    MinimalStateEvent, RoomInfo,
};
use matrix_sdk_store_encryption::{Error as EncryptionError, StoreCipher};
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
    CanonicalJsonObject, EventId, MxcUri, OwnedEventId, OwnedUserId, RoomId, RoomVersionId, UserId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
use tracing::{debug, warn};
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;

use crate::safe_encode::SafeEncode;

#[derive(Clone, Serialize, Deserialize)]
struct StoreKeyWrapper(Vec<u8>);

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbStateStoreError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Encryption(#[from] EncryptionError),
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error(transparent)]
    StoreError(#[from] StoreError),
    #[error("Can't migrate {name} from {old_version} to {new_version} without deleting data. See MigrationConflictStrategy for ways to configure.")]
    MigrationConflict { name: String, old_version: f64, new_version: f64 },
}

/// Sometimes Migrations can't proceed without having to drop existing
/// data. This allows you to configure, how these cases should be handled.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MigrationConflictStrategy {
    /// Just drop the data, we don't care that we have to sync again
    Drop,
    /// Raise a [`IndexeddbStateStoreError::MigrationConflict`] error with the
    /// path to the DB in question. The caller then has to take care about
    /// what they want to do and try again after.
    Raise,
    /// Default.
    BackupAndDrop,
}

impl From<indexed_db_futures::web_sys::DomException> for IndexeddbStateStoreError {
    fn from(frm: indexed_db_futures::web_sys::DomException) -> IndexeddbStateStoreError {
        IndexeddbStateStoreError::DomException {
            name: frm.name(),
            message: frm.message(),
            code: frm.code(),
        }
    }
}

impl From<IndexeddbStateStoreError> for StoreError {
    fn from(e: IndexeddbStateStoreError) -> Self {
        match e {
            IndexeddbStateStoreError::Json(e) => StoreError::Json(e),
            IndexeddbStateStoreError::StoreError(e) => e,
            IndexeddbStateStoreError::Encryption(e) => StoreError::Encryption(e),
            _ => StoreError::backend(e),
        }
    }
}

#[allow(non_snake_case)]
mod KEYS {
    // STORES

    pub const CURRENT_DB_VERSION: f64 = 1.2;
    pub const CURRENT_META_DB_VERSION: f64 = 2.0;

    pub const INTERNAL_STATE: &str = "matrix-sdk-state";
    pub const BACKUPS_META: &str = "backups";

    pub const SESSION: &str = "session";
    pub const ACCOUNT_DATA: &str = "account_data";

    pub const MEMBERS: &str = "members";
    pub const PROFILES: &str = "profiles";
    pub const DISPLAY_NAMES: &str = "display_names";
    pub const JOINED_USER_IDS: &str = "joined_user_ids";
    pub const INVITED_USER_IDS: &str = "invited_user_ids";

    pub const ROOM_STATE: &str = "room_state";
    pub const ROOM_INFOS: &str = "room_infos";
    pub const PRESENCE: &str = "presence";
    pub const ROOM_ACCOUNT_DATA: &str = "room_account_data";

    pub const STRIPPED_ROOM_INFOS: &str = "stripped_room_infos";
    pub const STRIPPED_MEMBERS: &str = "stripped_members";
    pub const STRIPPED_ROOM_STATE: &str = "stripped_room_state";
    pub const STRIPPED_JOINED_USER_IDS: &str = "stripped_joined_user_ids";
    pub const STRIPPED_INVITED_USER_IDS: &str = "stripped_invited_user_ids";

    pub const ROOM_USER_RECEIPTS: &str = "room_user_receipts";
    pub const ROOM_EVENT_RECEIPTS: &str = "room_event_receipts";

    pub const MEDIA: &str = "media";

    pub const CUSTOM: &str = "custom";

    pub const SYNC_TOKEN: &str = "sync_token";

    /// All names of the state stores for convenience.
    pub const ALL_STORES: &[&str] = &[
        SESSION,
        ACCOUNT_DATA,
        MEMBERS,
        PROFILES,
        DISPLAY_NAMES,
        JOINED_USER_IDS,
        INVITED_USER_IDS,
        ROOM_STATE,
        ROOM_INFOS,
        PRESENCE,
        ROOM_ACCOUNT_DATA,
        STRIPPED_ROOM_INFOS,
        STRIPPED_MEMBERS,
        STRIPPED_ROOM_STATE,
        STRIPPED_JOINED_USER_IDS,
        STRIPPED_INVITED_USER_IDS,
        ROOM_USER_RECEIPTS,
        ROOM_EVENT_RECEIPTS,
        MEDIA,
        CUSTOM,
        SYNC_TOKEN,
    ];

    // static keys

    pub const STORE_KEY: &str = "store_key";
    pub const FILTER: &str = "filter";
}

pub use KEYS::ALL_STORES;

fn drop_stores(db: &IdbDatabase) -> Result<(), JsValue> {
    for name in ALL_STORES {
        db.delete_object_store(name)?;
    }
    Ok(())
}

fn create_stores(db: &IdbDatabase) -> Result<(), JsValue> {
    for name in ALL_STORES {
        db.create_object_store(name)?;
    }
    Ok(())
}

async fn backup(source: &IdbDatabase, meta: &IdbDatabase) -> Result<()> {
    let now = JsDate::now();
    let backup_name = format!("backup-{}-{now}", source.name());

    let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&backup_name, source.version())?;
    db_req.set_on_upgrade_needed(Some(move |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        // migrating to version 1
        let db = evt.db();
        for name in ALL_STORES {
            db.create_object_store(name)?;
        }
        Ok(())
    }));
    let target = db_req.into_future().await?;

    for name in ALL_STORES {
        let tx = target.transaction_on_one_with_mode(name, IdbTransactionMode::Readwrite)?;

        let obj = tx.object_store(name)?;

        if let Some(curs) = source
            .transaction_on_one_with_mode(name, IdbTransactionMode::Readonly)?
            .object_store(name)?
            .open_cursor()?
            .await?
        {
            while let Some(key) = curs.key() {
                obj.put_key_val(&key, &curs.value())?;

                curs.continue_cursor()?.await?;
            }
        }

        tx.await.into_result()?;
    }

    let tx =
        meta.transaction_on_one_with_mode(KEYS::BACKUPS_META, IdbTransactionMode::Readwrite)?;
    let backup_store = tx.object_store(KEYS::BACKUPS_META)?;
    backup_store.put_key_val(&JsValue::from_f64(now), &JsValue::from_str(&backup_name))?;

    tx.await;

    Ok(())
}

fn serialize_event(store_cipher: Option<&StoreCipher>, event: &impl Serialize) -> Result<JsValue> {
    Ok(match store_cipher {
        Some(cipher) => JsValue::from_serde(&cipher.encrypt_value_typed(event)?)?,
        None => JsValue::from_serde(event)?,
    })
}

fn deserialize_event<T: DeserializeOwned>(
    store_cipher: Option<&StoreCipher>,
    event: JsValue,
) -> Result<T> {
    match store_cipher {
        Some(cipher) => Ok(cipher.decrypt_value_typed(event.into_serde()?)?),
        None => Ok(event.into_serde()?),
    }
}

async fn v1_2_fix_store(
    store: &IdbObjectStore<'_>,
    store_cipher: Option<&StoreCipher>,
) -> Result<()> {
    fn maybe_fix_json(raw_json: &RawJsonValue) -> Result<Option<JsonValue>> {
        let json = raw_json.get();

        if json.contains(r#""content":null"#) {
            let mut value: JsonValue = serde_json::from_str(json)?;
            if let Some(content) = value.get_mut("content") {
                if matches!(content, JsonValue::Null) {
                    *content = JsonValue::Object(Default::default());
                    return Ok(Some(value));
                }
            }
        }

        Ok(None)
    }

    let cursor = store.open_cursor()?.await?;

    if let Some(cursor) = cursor {
        loop {
            let raw_json: Box<RawJsonValue> = deserialize_event(store_cipher, cursor.value())?;

            if let Some(fixed_json) = maybe_fix_json(&raw_json)? {
                cursor.update(&serialize_event(store_cipher, &fixed_json)?)?.await?;
            }

            if !cursor.continue_cursor()?.await? {
                break;
            }
        }
    }

    Ok(())
}

async fn migrate_to_v1_2(db: &IdbDatabase, store_cipher: Option<&StoreCipher>) -> Result<()> {
    let tx = db.transaction_on_multi_with_mode(
        &[KEYS::ROOM_STATE, KEYS::ROOM_INFOS],
        IdbTransactionMode::Readwrite,
    )?;

    v1_2_fix_store(&tx.object_store(KEYS::ROOM_STATE)?, store_cipher).await?;
    v1_2_fix_store(&tx.object_store(KEYS::ROOM_INFOS)?, store_cipher).await?;

    tx.await.into_result().map_err(|e| e.into())
}

/// Builder for [`IndexeddbStateStore`].
#[derive(Debug)]
pub struct IndexeddbStateStoreBuilder {
    name: Option<String>,
    passphrase: Option<String>,
    migration_conflict_strategy: MigrationConflictStrategy,
}

impl IndexeddbStateStoreBuilder {
    fn new() -> Self {
        Self {
            name: None,
            passphrase: None,
            migration_conflict_strategy: MigrationConflictStrategy::BackupAndDrop,
        }
    }

    /// Set the name for the indexeddb store to use, `state` is none given.
    pub fn name(mut self, value: String) -> Self {
        self.name = Some(value);
        self
    }

    /// Set the password the indexeddb should be encrypted with.
    ///
    /// If not given, the DB is not encrypted.
    pub fn passphrase(mut self, value: String) -> Self {
        self.passphrase = Some(value);
        self
    }

    /// The strategy to use when a merge conflict is found.
    ///
    /// See [`MigrationConflictStrategy`] for details.
    pub fn migration_conflict_strategy(mut self, value: MigrationConflictStrategy) -> Self {
        self.migration_conflict_strategy = value;
        self
    }

    pub async fn build(self) -> Result<IndexeddbStateStore> {
        let migration_strategy = self.migration_conflict_strategy.clone();
        let name = self.name.unwrap_or_else(|| "state".to_owned());

        let meta_name = format!("{name}::{}", KEYS::INTERNAL_STATE);

        let mut db_req: OpenDbRequest =
            IdbDatabase::open_f64(&meta_name, KEYS::CURRENT_META_DB_VERSION)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            let db = evt.db();
            if evt.old_version() < 1.0 {
                // migrating to version 1

                db.create_object_store(KEYS::INTERNAL_STATE)?;
                db.create_object_store(KEYS::BACKUPS_META)?;
            } else if evt.old_version() < 2.0 {
                db.create_object_store(KEYS::BACKUPS_META)?;
            }
            Ok(())
        }));

        let meta_db: IdbDatabase = db_req.into_future().await?;

        let store_cipher = if let Some(passphrase) = &self.passphrase {
            let tx: IdbTransaction<'_> = meta_db.transaction_on_one_with_mode(
                KEYS::INTERNAL_STATE,
                IdbTransactionMode::Readwrite,
            )?;
            let ob = tx.object_store(KEYS::INTERNAL_STATE)?;

            let cipher = if let Some(StoreKeyWrapper(inner)) = ob
                .get(&JsValue::from_str(KEYS::STORE_KEY))?
                .await?
                .map(|v| v.into_serde())
                .transpose()?
            {
                StoreCipher::import(passphrase, &inner)?
            } else {
                let cipher = StoreCipher::new()?;
                #[cfg(not(test))]
                let export = cipher.export(passphrase)?;
                #[cfg(test)]
                let export = cipher._insecure_export_fast_for_testing(passphrase)?;
                ob.put_key_val(
                    &JsValue::from_str(KEYS::STORE_KEY),
                    &JsValue::from_serde(&StoreKeyWrapper(export))?,
                )?;
                cipher
            };

            tx.await.into_result()?;
            Some(Arc::new(cipher))
        } else {
            None
        };

        let mut recreate_stores = false;
        {
            // checkup up in a separate call, whether we have to backup or do anything else
            // to the db. Unfortunately the set_on_upgrade_needed doesn't allow async fn
            // which we need to execute the backup.
            let has_store_cipher = store_cipher.is_some();
            let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&name, 1.0)?;
            let created = Arc::new(AtomicBool::new(false));
            let created_inner = created.clone();

            db_req.set_on_upgrade_needed(Some(
                move |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
                    // in case this is a fresh db, we dont't want to trigger
                    // further migrations other than just creating the full
                    // schema.
                    if evt.old_version() < 1.0 {
                        create_stores(evt.db())?;
                        created_inner.store(true, Ordering::Relaxed);
                    }
                    Ok(())
                },
            ));

            let pre_db = db_req.into_future().await?;
            let old_version = pre_db.version();

            if created.load(Ordering::Relaxed) {
                // this is a fresh DB, nothing to do
            } else if old_version == 1.0 && has_store_cipher {
                match migration_strategy {
                    MigrationConflictStrategy::BackupAndDrop => {
                        backup(&pre_db, &meta_db).await?;
                        recreate_stores = true;
                    }
                    MigrationConflictStrategy::Drop => {
                        recreate_stores = true;
                    }
                    MigrationConflictStrategy::Raise => {
                        return Err(IndexeddbStateStoreError::MigrationConflict {
                            name,
                            old_version,
                            new_version: KEYS::CURRENT_DB_VERSION,
                        })
                    }
                }
            } else if old_version < 1.2 {
                migrate_to_v1_2(&pre_db, store_cipher.as_deref()).await?;
            } else {
                // Nothing to be done
            }
        }

        let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&name, KEYS::CURRENT_DB_VERSION)?;
        db_req.set_on_upgrade_needed(Some(
            move |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
                // changing the format can only happen in the upgrade procedure
                if recreate_stores {
                    drop_stores(evt.db())?;
                    create_stores(evt.db())?;
                }
                Ok(())
            },
        ));

        let db = db_req.into_future().await?;
        Ok(IndexeddbStateStore { name, inner: db, meta: meta_db, store_cipher })
    }
}

pub struct IndexeddbStateStore {
    name: String,
    pub(crate) inner: IdbDatabase,
    pub(crate) meta: IdbDatabase,
    pub(crate) store_cipher: Option<Arc<StoreCipher>>,
}

impl std::fmt::Debug for IndexeddbStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbStateStore").field("name", &self.name).finish()
    }
}

type Result<A, E = IndexeddbStateStoreError> = std::result::Result<A, E>;

impl IndexeddbStateStore {
    /// Generate a IndexeddbStateStoreBuilder with default parameters
    pub fn builder() -> IndexeddbStateStoreBuilder {
        IndexeddbStateStoreBuilder::new()
    }

    /// Whether this database has any migration backups
    pub async fn has_backups(&self) -> Result<bool> {
        Ok(self
            .meta
            .transaction_on_one_with_mode(KEYS::BACKUPS_META, IdbTransactionMode::Readonly)?
            .object_store(KEYS::BACKUPS_META)?
            .count()?
            .await?
            > 0)
    }

    /// What's the database name of the latest backup<
    pub async fn latest_backup(&self) -> Result<Option<String>> {
        Ok(self
            .meta
            .transaction_on_one_with_mode(KEYS::BACKUPS_META, IdbTransactionMode::Readonly)?
            .object_store(KEYS::BACKUPS_META)?
            .open_cursor_with_direction(indexed_db_futures::prelude::IdbCursorDirection::Prev)?
            .await?
            .and_then(|c| c.value().as_string()))
    }

    fn serialize_event(&self, event: &impl Serialize) -> Result<JsValue> {
        serialize_event(self.store_cipher.as_deref(), event)
    }

    fn deserialize_event<T: DeserializeOwned>(&self, event: JsValue) -> Result<T> {
        deserialize_event(self.store_cipher.as_deref(), event)
    }

    fn encode_key<T>(&self, table_name: &str, key: T) -> JsValue
    where
        T: SafeEncode,
    {
        match &self.store_cipher {
            Some(cipher) => key.encode_secure(table_name, cipher),
            None => key.encode(),
        }
    }

    fn encode_to_range<T>(&self, table_name: &str, key: T) -> Result<IdbKeyRange>
    where
        T: SafeEncode,
    {
        match &self.store_cipher {
            Some(cipher) => key.encode_to_range_secure(table_name, cipher),
            None => key.encode_to_range(),
        }
        .map_err(|e| IndexeddbStateStoreError::StoreError(StoreError::Backend(anyhow!(e).into())))
    }

    pub async fn get_user_ids_stream(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        Ok([
            self.get_invited_user_ids_inner(room_id).await?,
            self.get_joined_user_ids_inner(room_id).await?,
        ]
        .concat())
    }

    pub async fn get_invited_user_ids_inner(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let range = self.encode_to_range(KEYS::INVITED_USER_IDS, room_id)?;
        let entries = self
            .inner
            .transaction_on_one_with_mode(KEYS::INVITED_USER_IDS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::INVITED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<OwnedUserId>(f).ok())
            .collect::<Vec<_>>();

        Ok(entries)
    }

    pub async fn get_joined_user_ids_inner(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let range = self.encode_to_range(KEYS::JOINED_USER_IDS, room_id)?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::JOINED_USER_IDS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::JOINED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<OwnedUserId>(f).ok())
            .collect::<Vec<_>>())
    }

    pub async fn get_stripped_user_ids_stream(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        Ok([
            self.get_stripped_invited_user_ids(room_id).await?,
            self.get_stripped_joined_user_ids(room_id).await?,
        ]
        .concat())
    }

    pub async fn get_stripped_invited_user_ids(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<OwnedUserId>> {
        let range = self.encode_to_range(KEYS::STRIPPED_INVITED_USER_IDS, room_id)?;
        let entries = self
            .inner
            .transaction_on_one_with_mode(
                KEYS::STRIPPED_INVITED_USER_IDS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(KEYS::STRIPPED_INVITED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<OwnedUserId>(f).ok())
            .collect::<Vec<_>>();

        Ok(entries)
    }

    pub async fn get_stripped_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let range = self.encode_to_range(KEYS::STRIPPED_JOINED_USER_IDS, room_id)?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(
                KEYS::STRIPPED_JOINED_USER_IDS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(KEYS::STRIPPED_JOINED_USER_IDS)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<OwnedUserId>(f).ok())
            .collect::<Vec<_>>())
    }

    async fn get_custom_value_for_js(&self, jskey: &JsValue) -> Result<Option<Vec<u8>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::CUSTOM, IdbTransactionMode::Readonly)?
            .object_store(KEYS::CUSTOM)?
            .get(jskey)?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }
}

// Small hack to have the following macro invocation act as the appropriate
// trait impl block on wasm, but still be compiled on non-wasm as a regular
// impl block otherwise.
//
// The trait impl doesn't compile on non-wasm due to unfulfilled trait bounds,
// this hack allows us to still have most of rust-analyzer's IDE functionality
// within the impl block without having to set it up to check things against
// the wasm target (which would disable many other parts of the codebase).
#[cfg(target_arch = "wasm32")]
macro_rules! impl_state_store {
    ( $($body:tt)* ) => {
        #[async_trait(?Send)]
        impl StateStore for IndexeddbStateStore {
            type Error = IndexeddbStateStoreError;

            $($body)*
        }
    };
}

#[cfg(not(target_arch = "wasm32"))]
macro_rules! impl_state_store {
    ( $($body:tt)* ) => {
        impl IndexeddbStateStore {
            $($body)*
        }
    };
}

impl_state_store! {
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readwrite)?;

        let obj = tx.object_store(KEYS::SESSION)?;

        obj.put_key_val(
            &self.encode_key(KEYS::FILTER, (KEYS::FILTER, filter_name)),
            &self.serialize_event(&filter_id)?,
        )?;

        tx.await.into_result()?;

        Ok(())
    }

    async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readonly)?
            .object_store(KEYS::SESSION)?
            .get(&self.encode_key(KEYS::FILTER, (KEYS::FILTER, filter_name)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    async fn get_sync_token(&self) -> Result<Option<String>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::SYNC_TOKEN, IdbTransactionMode::Readonly)?
            .object_store(KEYS::SYNC_TOKEN)?
            .get(&JsValue::from_str(KEYS::SYNC_TOKEN))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let mut stores: HashSet<&'static str> = [
            (changes.sync_token.is_some(), KEYS::SYNC_TOKEN),
            (changes.session.is_some(), KEYS::SESSION),
            (!changes.ambiguity_maps.is_empty(), KEYS::DISPLAY_NAMES),
            (!changes.account_data.is_empty(), KEYS::ACCOUNT_DATA),
            (!changes.presence.is_empty(), KEYS::PRESENCE),
            (!changes.profiles.is_empty(), KEYS::PROFILES),
            (!changes.room_account_data.is_empty(), KEYS::ROOM_ACCOUNT_DATA),
            (!changes.receipts.is_empty(), KEYS::ROOM_EVENT_RECEIPTS),
            (!changes.stripped_state.is_empty(), KEYS::STRIPPED_ROOM_STATE),
        ]
        .iter()
        .filter_map(|(id, key)| if *id { Some(*key) } else { None })
        .collect();

        if !changes.state.is_empty() {
            stores.extend([KEYS::ROOM_STATE, KEYS::STRIPPED_ROOM_STATE]);
        }

        if !changes.redactions.is_empty() {
            stores.extend([KEYS::ROOM_STATE, KEYS::ROOM_INFOS]);
        }

        if !changes.room_infos.is_empty() || !changes.stripped_room_infos.is_empty() {
            stores.extend([KEYS::ROOM_INFOS, KEYS::STRIPPED_ROOM_INFOS]);
        }

        if !changes.members.is_empty() {
            stores.extend([
                KEYS::PROFILES,
                KEYS::MEMBERS,
                KEYS::INVITED_USER_IDS,
                KEYS::JOINED_USER_IDS,
                KEYS::STRIPPED_MEMBERS,
                KEYS::STRIPPED_INVITED_USER_IDS,
                KEYS::STRIPPED_JOINED_USER_IDS,
            ])
        }

        if !changes.stripped_members.is_empty() {
            stores.extend([
                KEYS::STRIPPED_MEMBERS,
                KEYS::STRIPPED_INVITED_USER_IDS,
                KEYS::STRIPPED_JOINED_USER_IDS,
            ])
        }

        if !changes.receipts.is_empty() {
            stores.extend([KEYS::ROOM_EVENT_RECEIPTS, KEYS::ROOM_USER_RECEIPTS])
        }

        if stores.is_empty() {
            // nothing to do, quit early
            return Ok(());
        }

        let stores: Vec<&'static str> = stores.into_iter().collect();
        let tx =
            self.inner.transaction_on_multi_with_mode(&stores, IdbTransactionMode::Readwrite)?;

        if let Some(s) = &changes.sync_token {
            tx.object_store(KEYS::SYNC_TOKEN)?
                .put_key_val(&JsValue::from_str(KEYS::SYNC_TOKEN), &self.serialize_event(s)?)?;
        }

        if !changes.ambiguity_maps.is_empty() {
            let store = tx.object_store(KEYS::DISPLAY_NAMES)?;
            for (room_id, ambiguity_maps) in &changes.ambiguity_maps {
                for (display_name, map) in ambiguity_maps {
                    let key = self.encode_key(KEYS::DISPLAY_NAMES, (room_id, display_name));

                    store.put_key_val(&key, &self.serialize_event(&map)?)?;
                }
            }
        }

        if !changes.account_data.is_empty() {
            let store = tx.object_store(KEYS::ACCOUNT_DATA)?;
            for (event_type, event) in &changes.account_data {
                store.put_key_val(
                    &self.encode_key(KEYS::ACCOUNT_DATA, event_type),
                    &self.serialize_event(&event)?,
                )?;
            }
        }

        if !changes.room_account_data.is_empty() {
            let store = tx.object_store(KEYS::ROOM_ACCOUNT_DATA)?;
            for (room, events) in &changes.room_account_data {
                for (event_type, event) in events {
                    let key = self.encode_key(KEYS::ROOM_ACCOUNT_DATA, (room, event_type));
                    store.put_key_val(&key, &self.serialize_event(&event)?)?;
                }
            }
        }

        if !changes.state.is_empty() {
            let state = tx.object_store(KEYS::ROOM_STATE)?;
            let stripped_state = tx.object_store(KEYS::STRIPPED_ROOM_STATE)?;
            for (room, event_types) in &changes.state {
                for (event_type, events) in event_types {
                    for (state_key, event) in events {
                        let key = self.encode_key(KEYS::ROOM_STATE, (room, event_type, state_key));
                        state.put_key_val(&key, &self.serialize_event(&event)?)?;
                        stripped_state.delete(&key)?;
                    }
                }
            }
        }

        if !changes.room_infos.is_empty() {
            let room_infos = tx.object_store(KEYS::ROOM_INFOS)?;
            let stripped_room_infos = tx.object_store(KEYS::STRIPPED_ROOM_INFOS)?;
            for (room_id, room_info) in &changes.room_infos {
                room_infos.put_key_val(
                    &self.encode_key(KEYS::ROOM_INFOS, room_id),
                    &self.serialize_event(&room_info)?,
                )?;
                stripped_room_infos.delete(&self.encode_key(KEYS::STRIPPED_ROOM_INFOS, room_id))?;
            }
        }

        if !changes.presence.is_empty() {
            let store = tx.object_store(KEYS::PRESENCE)?;
            for (sender, event) in &changes.presence {
                store.put_key_val(
                    &self.encode_key(KEYS::PRESENCE, sender),
                    &self.serialize_event(&event)?,
                )?;
            }
        }

        if !changes.stripped_room_infos.is_empty() {
            let stripped_room_infos = tx.object_store(KEYS::STRIPPED_ROOM_INFOS)?;
            let room_infos = tx.object_store(KEYS::ROOM_INFOS)?;
            for (room_id, info) in &changes.stripped_room_infos {
                stripped_room_infos.put_key_val(
                    &self.encode_key(KEYS::STRIPPED_ROOM_INFOS, room_id),
                    &self.serialize_event(&info)?,
                )?;
                room_infos.delete(&self.encode_key(KEYS::ROOM_INFOS, room_id))?;
            }
        }

        if !changes.stripped_members.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_MEMBERS)?;
            let joined = tx.object_store(KEYS::STRIPPED_JOINED_USER_IDS)?;
            let invited = tx.object_store(KEYS::STRIPPED_INVITED_USER_IDS)?;
            for (room, raw_events) in &changes.stripped_members {
                for raw_event in raw_events.values() {
                    let event = match raw_event.deserialize() {
                        Ok(ev) => ev,
                        Err(e) => {
                            let event_id: Option<String> =
                                raw_event.get_field("event_id").ok().flatten();
                            debug!(event_id, "Failed to deserialize stripped member event: {e}");
                            continue;
                        }
                    };

                    let key = (room, &event.state_key);

                    match event.content.membership {
                        MembershipState::Join => {
                            joined.put_key_val_owned(
                                &self.encode_key(KEYS::STRIPPED_JOINED_USER_IDS, key),
                                &self.serialize_event(&event.state_key)?,
                            )?;
                            invited
                                .delete(&self.encode_key(KEYS::STRIPPED_INVITED_USER_IDS, key))?;
                        }
                        MembershipState::Invite => {
                            invited.put_key_val_owned(
                                &self.encode_key(KEYS::STRIPPED_INVITED_USER_IDS, key),
                                &self.serialize_event(&event.state_key)?,
                            )?;
                            joined.delete(&self.encode_key(KEYS::STRIPPED_JOINED_USER_IDS, key))?;
                        }
                        _ => {
                            joined.delete(&self.encode_key(KEYS::STRIPPED_JOINED_USER_IDS, key))?;
                            invited
                                .delete(&self.encode_key(KEYS::STRIPPED_INVITED_USER_IDS, key))?;
                        }
                    }
                    store.put_key_val(
                        &self.encode_key(KEYS::STRIPPED_MEMBERS, key),
                        &self.serialize_event(&raw_event)?,
                    )?;
                }
            }
        }

        if !changes.stripped_state.is_empty() {
            let store = tx.object_store(KEYS::STRIPPED_ROOM_STATE)?;
            for (room, event_types) in &changes.stripped_state {
                for (event_type, events) in event_types {
                    for (state_key, event) in events {
                        let key = self
                            .encode_key(KEYS::STRIPPED_ROOM_STATE, (room, event_type, state_key));
                        store.put_key_val(&key, &self.serialize_event(&event)?)?;
                    }
                }
            }
        }

        if !changes.members.is_empty() {
            let profiles = tx.object_store(KEYS::PROFILES)?;
            let joined = tx.object_store(KEYS::JOINED_USER_IDS)?;
            let invited = tx.object_store(KEYS::INVITED_USER_IDS)?;
            let members = tx.object_store(KEYS::MEMBERS)?;
            let stripped_members = tx.object_store(KEYS::STRIPPED_MEMBERS)?;
            let stripped_joined = tx.object_store(KEYS::STRIPPED_JOINED_USER_IDS)?;
            let stripped_invited = tx.object_store(KEYS::STRIPPED_INVITED_USER_IDS)?;

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
                        .delete(&self.encode_key(KEYS::STRIPPED_JOINED_USER_IDS, key))?;
                    stripped_invited
                        .delete(&self.encode_key(KEYS::STRIPPED_INVITED_USER_IDS, key))?;

                    match event.membership() {
                        MembershipState::Join => {
                            joined.put_key_val_owned(
                                &self.encode_key(KEYS::JOINED_USER_IDS, key),
                                &self.serialize_event(event.state_key())?,
                            )?;
                            invited.delete(&self.encode_key(KEYS::INVITED_USER_IDS, key))?;
                        }
                        MembershipState::Invite => {
                            invited.put_key_val_owned(
                                &self.encode_key(KEYS::INVITED_USER_IDS, key),
                                &self.serialize_event(event.state_key())?,
                            )?;
                            joined.delete(&self.encode_key(KEYS::JOINED_USER_IDS, key))?;
                        }
                        _ => {
                            joined.delete(&self.encode_key(KEYS::JOINED_USER_IDS, key))?;
                            invited.delete(&self.encode_key(KEYS::INVITED_USER_IDS, key))?;
                        }
                    }

                    members.put_key_val_owned(
                        &self.encode_key(KEYS::MEMBERS, key),
                        &self.serialize_event(&raw_event)?,
                    )?;
                    stripped_members.delete(&self.encode_key(KEYS::STRIPPED_MEMBERS, key))?;

                    if let Some(profile) = profile_changes.and_then(|p| p.get(event.state_key())) {
                        profiles.put_key_val_owned(
                            &self.encode_key(KEYS::PROFILES, key),
                            &self.serialize_event(&profile)?,
                        )?;
                    }
                }
            }
        }

        if !changes.receipts.is_empty() {
            let room_user_receipts = tx.object_store(KEYS::ROOM_USER_RECEIPTS)?;
            let room_event_receipts = tx.object_store(KEYS::ROOM_EVENT_RECEIPTS)?;

            for (room, content) in &changes.receipts {
                for (event_id, receipts) in &content.0 {
                    for (receipt_type, receipts) in receipts {
                        for (user_id, receipt) in receipts {
                            let key = match receipt.thread.as_str() {
                                Some(thread_id) => self.encode_key(
                                    KEYS::ROOM_USER_RECEIPTS,
                                    (room, receipt_type, thread_id, user_id),
                                ),
                                None => self.encode_key(
                                    KEYS::ROOM_USER_RECEIPTS,
                                    (room, receipt_type, user_id),
                                ),
                            };

                            if let Some((old_event, _)) =
                                room_user_receipts.get(&key)?.await?.and_then(|f| {
                                    self.deserialize_event::<(OwnedEventId, Receipt)>(f).ok()
                                })
                            {
                                let key = match receipt.thread.as_str() {
                                    Some(thread_id) => self.encode_key(
                                        KEYS::ROOM_EVENT_RECEIPTS,
                                        (room, receipt_type, thread_id, old_event, user_id),
                                    ),
                                    None => self.encode_key(
                                        KEYS::ROOM_EVENT_RECEIPTS,
                                        (room, receipt_type, old_event, user_id),
                                    ),
                                };
                                room_event_receipts.delete(&key)?;
                            }

                            room_user_receipts
                                .put_key_val(&key, &self.serialize_event(&(event_id, receipt))?)?;

                            // Add the receipt to the room event receipts
                            let key = match receipt.thread.as_str() {
                                Some(thread_id) => self.encode_key(
                                    KEYS::ROOM_EVENT_RECEIPTS,
                                    (room, receipt_type, thread_id, event_id, user_id),
                                ),
                                None => self.encode_key(
                                    KEYS::ROOM_EVENT_RECEIPTS,
                                    (room, receipt_type, event_id, user_id),
                                ),
                            };
                            room_event_receipts
                                .put_key_val(&key, &self.serialize_event(&(user_id, receipt))?)?;
                        }
                    }
                }
            }
        }

        if !changes.redactions.is_empty() {
            let state = tx.object_store(KEYS::ROOM_STATE)?;
            let room_info = tx.object_store(KEYS::ROOM_INFOS)?;

            for (room_id, redactions) in &changes.redactions {
                let range = self.encode_to_range(KEYS::ROOM_STATE, room_id)?;
                let Some(cursor) = state.open_cursor_with_range(&range)?.await? else { continue };

                let mut room_version = None;

                while let Some(key) = cursor.key() {
                    let raw_evt =
                        self.deserialize_event::<Raw<AnySyncStateEvent>>(cursor.value())?;
                    if let Ok(Some(event_id)) = raw_evt.get_field::<OwnedEventId>("event_id") {
                        if let Some(redaction) = redactions.get(&event_id) {
                            let version = {
                                if room_version.is_none() {
                                    room_version.replace(room_info
                                        .get(&self.encode_key(KEYS::ROOM_INFOS, room_id))?
                                        .await?
                                        .and_then(|f| self.deserialize_event::<RoomInfo>(f).ok())
                                        .and_then(|info| info.room_version().cloned())
                                        .unwrap_or_else(|| {
                                            warn!(?room_id, "Unable to find the room version, assume version 9");
                                            RoomVersionId::V9
                                        })
                                    );
                                }
                                room_version.as_ref().unwrap()
                            };

                            let redacted = redact(
                                raw_evt.deserialize_as::<CanonicalJsonObject>()?,
                                version,
                                Some(redaction.try_into()?),
                            )
                            .map_err(StoreError::Redaction)?;
                            state.put_key_val(&key, &self.serialize_event(&redacted)?)?;
                        }
                    }

                    // move forward.
                    cursor.advance(1)?.await?;
                }
            }
        }

        tx.await.into_result().map_err(|e| e.into())
    }

     async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::PRESENCE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::PRESENCE)?
            .get(&self.encode_key(KEYS::PRESENCE, user_id))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

     async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_STATE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_STATE)?
            .get(&self.encode_key(KEYS::ROOM_STATE, (room_id, event_type, state_key)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

     async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        let range = self.encode_to_range(KEYS::ROOM_STATE, (room_id, event_type))?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::ROOM_STATE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_STATE)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event(f).ok())
            .collect::<Vec<_>>())
    }

     async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalStateEvent<RoomMemberEventContent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::PROFILES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::PROFILES)?
            .get(&self.encode_key(KEYS::PROFILES, (room_id, user_id)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

     async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<RawMemberEvent>> {
        if let Some(e) = self
            .inner
            .transaction_on_one_with_mode(KEYS::STRIPPED_MEMBERS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::STRIPPED_MEMBERS)?
            .get(&self.encode_key(KEYS::STRIPPED_MEMBERS, (room_id, state_key)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?
        {
            Ok(Some(RawMemberEvent::Stripped(e)))
        } else if let Some(e) = self
            .inner
            .transaction_on_one_with_mode(KEYS::MEMBERS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::MEMBERS)?
            .get(&self.encode_key(KEYS::MEMBERS, (room_id, state_key)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()?
        {
            Ok(Some(RawMemberEvent::Sync(e)))
        } else {
            Ok(None)
        }
    }

     async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        let entries: Vec<_> = self
            .inner
            .transaction_on_one_with_mode(KEYS::ROOM_INFOS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_INFOS)?
            .get_all()?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<RoomInfo>(f).ok())
            .collect();

        Ok(entries)
    }

     async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        let entries = self
            .inner
            .transaction_on_one_with_mode(KEYS::STRIPPED_ROOM_INFOS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::STRIPPED_ROOM_INFOS)?
            .get_all()?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event(f).ok())
            .collect::<Vec<_>>();

        Ok(entries)
    }

     async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::DISPLAY_NAMES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::DISPLAY_NAMES)?
            .get(&self.encode_key(KEYS::DISPLAY_NAMES, (room_id, display_name)))?
            .await?
            .map(|f| self.deserialize_event::<BTreeSet<OwnedUserId>>(f))
            .unwrap_or_else(|| Ok(Default::default()))
    }

     async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ACCOUNT_DATA)?
            .get(&self.encode_key(KEYS::ACCOUNT_DATA, event_type))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

     async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_ACCOUNT_DATA)?
            .get(&self.encode_key(KEYS::ROOM_ACCOUNT_DATA, (room_id, event_type)))?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        let key = match thread.as_str() {
            Some(thread_id) => self
                .encode_key(KEYS::ROOM_USER_RECEIPTS, (room_id, receipt_type, thread_id, user_id)),
            None => self.encode_key(KEYS::ROOM_USER_RECEIPTS, (room_id, receipt_type, user_id)),
        };
        self.inner
            .transaction_on_one_with_mode(KEYS::ROOM_USER_RECEIPTS, IdbTransactionMode::Readonly)?
            .object_store(KEYS::ROOM_USER_RECEIPTS)?
            .get(&key)?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        let range = match thread.as_str() {
            Some(thread_id) => self.encode_to_range(
                KEYS::ROOM_EVENT_RECEIPTS,
                (room_id, receipt_type, thread_id, event_id),
            ),
            None => {
                self.encode_to_range(KEYS::ROOM_EVENT_RECEIPTS, (room_id, receipt_type, event_id))
            }
        }?;
        let tx = self.inner.transaction_on_one_with_mode(
            KEYS::ROOM_EVENT_RECEIPTS,
            IdbTransactionMode::Readonly,
        )?;
        let store = tx.object_store(KEYS::ROOM_EVENT_RECEIPTS)?;

        Ok(store
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event(f).ok())
            .collect::<Vec<_>>())
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        let key = self
            .encode_key(KEYS::MEDIA, (request.source.unique_key(), request.format.unique_key()));
        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readwrite)?;

        tx.object_store(KEYS::MEDIA)?.put_key_val(&key, &self.serialize_event(&data)?)?;

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let key = self
            .encode_key(KEYS::MEDIA, (request.source.unique_key(), request.format.unique_key()));
        self.inner
            .transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readonly)?
            .object_store(KEYS::MEDIA)?
            .get(&key)?
            .await?
            .map(|f| self.deserialize_event(f))
            .transpose()
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let jskey = &JsValue::from_str(core::str::from_utf8(key).map_err(StoreError::Codec)?);
        self.get_custom_value_for_js(jskey).await
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        let jskey = JsValue::from_str(core::str::from_utf8(key).map_err(StoreError::Codec)?);

        let prev = self.get_custom_value_for_js(&jskey).await?;

        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::CUSTOM, IdbTransactionMode::Readwrite)?;

        tx.object_store(KEYS::CUSTOM)?.put_key_val(&jskey, &self.serialize_event(&value)?)?;

        tx.await.into_result().map_err(IndexeddbStateStoreError::from)?;
        Ok(prev)
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        let key = self
            .encode_key(KEYS::MEDIA, (request.source.unique_key(), request.format.unique_key()));
        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readwrite)?;

        tx.object_store(KEYS::MEDIA)?.delete(&key)?;

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let range = self.encode_to_range(KEYS::MEDIA, uri)?;
        let tx =
            self.inner.transaction_on_one_with_mode(KEYS::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(KEYS::MEDIA)?;

        for k in store.get_all_keys_with_key(&range)?.await?.iter() {
            store.delete(&k)?;
        }

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        let direct_stores = [KEYS::ROOM_INFOS, KEYS::STRIPPED_ROOM_INFOS];

        let prefixed_stores = [
            KEYS::MEMBERS,
            KEYS::PROFILES,
            KEYS::DISPLAY_NAMES,
            KEYS::INVITED_USER_IDS,
            KEYS::JOINED_USER_IDS,
            KEYS::ROOM_STATE,
            KEYS::ROOM_ACCOUNT_DATA,
            KEYS::ROOM_EVENT_RECEIPTS,
            KEYS::ROOM_USER_RECEIPTS,
            KEYS::STRIPPED_ROOM_STATE,
            KEYS::STRIPPED_MEMBERS,
        ];

        let all_stores = {
            let mut v = Vec::new();
            v.extend(prefixed_stores);
            v.extend(direct_stores);
            v
        };

        let tx = self
            .inner
            .transaction_on_multi_with_mode(&all_stores, IdbTransactionMode::Readwrite)?;

        for store_name in direct_stores {
            tx.object_store(store_name)?.delete(&self.encode_key(store_name, room_id))?;
        }

        for store_name in prefixed_stores {
            let store = tx.object_store(store_name)?;
            let range = self.encode_to_range(store_name, room_id)?;
            for key in store.get_all_keys_with_key(&range)?.await?.iter() {
                store.delete(&key)?;
            }
        }
        tx.await.into_result().map_err(|e| e.into())
    }

    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let ids: Vec<OwnedUserId> = self.get_stripped_user_ids_stream(room_id).await?;
        if !ids.is_empty() {
            return Ok(ids);
        }
        self.get_user_ids_stream(room_id).await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let ids: Vec<OwnedUserId> = self.get_stripped_invited_user_ids(room_id).await?;
        if !ids.is_empty() {
            return Ok(ids);
        }
        self.get_invited_user_ids_inner(room_id).await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let ids: Vec<OwnedUserId> = self.get_stripped_joined_user_ids(room_id).await?;
        if !ids.is_empty() {
            return Ok(ids);
        }
        self.get_joined_user_ids_inner(room_id).await
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use matrix_sdk_base::statestore_integration_tests;
    use uuid::Uuid;

    use super::{IndexeddbStateStore, Result};

    async fn get_store() -> Result<IndexeddbStateStore> {
        let db_name = format!("test-state-plain-{}", Uuid::new_v4().as_hyphenated());
        Ok(IndexeddbStateStore::builder().name(db_name).build().await?)
    }

    statestore_integration_tests!(with_media_tests);
}

#[cfg(all(test, target_arch = "wasm32"))]
mod encrypted_tests {
    #[cfg(target_arch = "wasm32")]
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use matrix_sdk_base::statestore_integration_tests;
    use uuid::Uuid;

    use super::{IndexeddbStateStore, Result};

    async fn get_store() -> Result<IndexeddbStateStore> {
        let db_name = format!("test-state-encrypted-{}", Uuid::new_v4().as_hyphenated());
        let passphrase = format!("some_passphrase-{}", Uuid::new_v4().as_hyphenated());
        Ok(IndexeddbStateStore::builder().name(db_name).passphrase(passphrase).build().await?)
    }

    statestore_integration_tests!(with_media_tests);
}

#[cfg(all(test, target_arch = "wasm32"))]
mod migration_tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use indexed_db_futures::prelude::*;
    use matrix_sdk_base::StateStore;
    use matrix_sdk_test::async_test;
    use ruma::{
        events::{AnySyncStateEvent, StateEventType},
        room_id,
    };
    use serde_json::json;
    use uuid::Uuid;
    use wasm_bindgen::JsValue;

    use super::{
        serialize_event, IndexeddbStateStore, IndexeddbStateStoreError, MigrationConflictStrategy,
        Result, ALL_STORES, KEYS,
    };
    use crate::safe_encode::SafeEncode;

    pub async fn create_fake_db(name: &str, version: f64) -> Result<IdbDatabase> {
        let mut db_req: OpenDbRequest = IdbDatabase::open_f64(name, version)?;
        db_req.set_on_upgrade_needed(Some(
            move |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
                // migrating to version 1
                let db = evt.db();
                for name in ALL_STORES {
                    db.create_object_store(name)?;
                }
                Ok(())
            },
        ));
        db_req.into_future().await.map_err(Into::into)
    }

    #[async_test]
    pub async fn test_no_upgrade() -> Result<()> {
        let name = format!("simple-1.1-no-cipher-{}", Uuid::new_v4().as_hyphenated().to_string());

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;
        // this didn't create any backup
        assert_eq!(store.has_backups().await?, false);
        // simple check that the layout exists.
        assert_eq!(store.get_sync_token().await?, None);
        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_v1_to_1_1_plain() -> Result<()> {
        let name =
            format!("migrating-1.1-no-cipher-{}", Uuid::new_v4().as_hyphenated().to_string());
        create_fake_db(&name, 1.0).await?;

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;
        // this didn't create any backup
        assert_eq!(store.has_backups().await?, false);
        assert_eq!(store.get_sync_token().await?, None);
        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_v1_to_1_1_with_pw() -> Result<()> {
        let name =
            format!("migrating-1.1-with-cipher-{}", Uuid::new_v4().as_hyphenated().to_string());
        let passphrase = "somepassphrase".to_owned();
        create_fake_db(&name, 1.0).await?;

        // this transparently migrates to the latest version
        let store =
            IndexeddbStateStore::builder().name(name).passphrase(passphrase).build().await?;
        // this creates a backup by default
        assert_eq!(store.has_backups().await?, true);
        assert!(store.latest_backup().await?.is_some(), "No backup_found");
        assert_eq!(store.get_sync_token().await?, None);
        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_v1_to_1_1_with_pw_drops() -> Result<()> {
        let name = format!(
            "migrating-1.1-with-cipher-drops-{}",
            Uuid::new_v4().as_hyphenated().to_string()
        );
        let passphrase = "some-other-passphrase".to_owned();
        create_fake_db(&name, 1.0).await?;

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder()
            .name(name)
            .passphrase(passphrase)
            .migration_conflict_strategy(MigrationConflictStrategy::Drop)
            .build()
            .await?;
        // this creates a backup by default
        assert_eq!(store.has_backups().await?, false);
        assert_eq!(store.get_sync_token().await?, None);
        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_v1_to_1_1_with_pw_raise() -> Result<()> {
        let name = format!(
            "migrating-1.1-with-cipher-raises-{}",
            Uuid::new_v4().as_hyphenated().to_string()
        );
        let passphrase = "some-other-passphrase".to_owned();
        create_fake_db(&name, 1.0).await?;

        // this transparently migrates to the latest version
        let store_res = IndexeddbStateStore::builder()
            .name(name)
            .passphrase(passphrase)
            .migration_conflict_strategy(MigrationConflictStrategy::Raise)
            .build()
            .await;

        if let Err(IndexeddbStateStoreError::MigrationConflict { .. }) = store_res {
            // all fine!
        } else {
            assert!(false, "Conflict didn't raise: {:?}", store_res)
        }
        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_to_v1_2() -> Result<()> {
        let name = format!("migrating-1.2-{}", Uuid::new_v4().as_hyphenated().to_string());
        // An event that fails to deserialize.
        let wrong_redacted_state_event = json!({
            "content": null,
            "event_id": "$wrongevent",
            "origin_server_ts": 1673887516047_u64,
            "sender": "@example:localhost",
            "state_key": "",
            "type": "m.room.topic",
            "unsigned": {
                "redacted_because": {
                    "type": "m.room.redaction",
                    "sender": "@example:localhost",
                    "content": {},
                    "redacts": "$wrongevent",
                    "origin_server_ts": 1673893816047_u64,
                    "unsigned": {},
                    "event_id": "$redactionevent",
                },
            },
        });
        serde_json::from_value::<AnySyncStateEvent>(wrong_redacted_state_event.clone())
            .unwrap_err();

        let room_id = room_id!("!some_room:localhost");

        // Populate DB with wrong event.
        {
            let db = create_fake_db(&name, 1.1).await?;
            let tx =
                db.transaction_on_one_with_mode(KEYS::ROOM_STATE, IdbTransactionMode::Readwrite)?;
            let state = tx.object_store(KEYS::ROOM_STATE)?;
            let key = (room_id, StateEventType::RoomTopic, "").encode();
            state.put_key_val(&key, &serialize_event(None, &wrong_redacted_state_event)?)?;
            tx.await.into_result()?;
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;
        let event =
            store.get_state_event(room_id, StateEventType::RoomTopic, "").await.unwrap().unwrap();
        event.deserialize().unwrap();

        Ok(())
    }
}
