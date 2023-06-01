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
    sync::Arc,
};

use anyhow::anyhow;
use async_trait::async_trait;
use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::prelude::*;
use matrix_sdk_base::{
    deserialized_responses::RawAnySyncOrStrippedState,
    media::{MediaRequest, UniqueKey},
    store::{StateChanges, StateStore, StoreError},
    MinimalStateEvent, RoomInfo, RoomMemberships, RoomState, StateStoreDataKey,
    StateStoreDataValue,
};
use matrix_sdk_store_encryption::{Error as EncryptionError, StoreCipher};
use ruma::{
    canonical_json::redact,
    events::{
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::member::{
            MembershipState, RoomMemberEventContent, StrippedRoomMemberEvent, SyncRoomMemberEvent,
        },
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnySyncStateEvent,
        GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType, SyncStateEvent,
    },
    serde::Raw,
    CanonicalJsonObject, EventId, MxcUri, OwnedEventId, OwnedUserId, RoomId, RoomVersionId, UserId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tracing::{debug, warn};
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;

mod migrations;

pub use self::migrations::MigrationConflictStrategy;
use self::migrations::{upgrade_inner_db, upgrade_meta_db};
use crate::safe_encode::SafeEncode;

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
    MigrationConflict { name: String, old_version: u32, new_version: u32 },
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

mod keys {
    pub const INTERNAL_STATE: &str = "matrix-sdk-state";
    pub const BACKUPS_META: &str = "backups";

    pub const ACCOUNT_DATA: &str = "account_data";

    pub const PROFILES: &str = "profiles";
    pub const DISPLAY_NAMES: &str = "display_names";
    pub const USER_IDS: &str = "user_ids";

    pub const ROOM_STATE: &str = "room_state";
    pub const ROOM_INFOS: &str = "room_infos";
    pub const PRESENCE: &str = "presence";
    pub const ROOM_ACCOUNT_DATA: &str = "room_account_data";

    pub const STRIPPED_ROOM_STATE: &str = "stripped_room_state";
    pub const STRIPPED_USER_IDS: &str = "stripped_user_ids";

    pub const ROOM_USER_RECEIPTS: &str = "room_user_receipts";
    pub const ROOM_EVENT_RECEIPTS: &str = "room_event_receipts";

    pub const MEDIA: &str = "media";

    pub const CUSTOM: &str = "custom";
    pub const KV: &str = "kv";

    /// All names of the current state stores for convenience.
    pub const ALL_STORES: &[&str] = &[
        ACCOUNT_DATA,
        PROFILES,
        DISPLAY_NAMES,
        USER_IDS,
        ROOM_STATE,
        ROOM_INFOS,
        PRESENCE,
        ROOM_ACCOUNT_DATA,
        STRIPPED_ROOM_STATE,
        STRIPPED_USER_IDS,
        ROOM_USER_RECEIPTS,
        ROOM_EVENT_RECEIPTS,
        MEDIA,
        CUSTOM,
        KV,
    ];

    // static keys

    pub const STORE_KEY: &str = "store_key";
}

pub use keys::ALL_STORES;

fn serialize_event(store_cipher: Option<&StoreCipher>, event: &impl Serialize) -> Result<JsValue> {
    Ok(match store_cipher {
        Some(cipher) => JsValue::from_serde(&cipher.encrypt_value_typed(event)?)?,
        None => JsValue::from_serde(event)?,
    })
}

fn deserialize_event<T: DeserializeOwned>(
    store_cipher: Option<&StoreCipher>,
    event: &JsValue,
) -> Result<T> {
    match store_cipher {
        Some(cipher) => Ok(cipher.decrypt_value_typed(event.into_serde()?)?),
        None => Ok(event.into_serde()?),
    }
}

fn encode_key<T>(store_cipher: Option<&StoreCipher>, table_name: &str, key: T) -> JsValue
where
    T: SafeEncode,
{
    match store_cipher {
        Some(cipher) => key.encode_secure(table_name, cipher),
        None => key.encode(),
    }
}

fn encode_to_range<T>(
    store_cipher: Option<&StoreCipher>,
    table_name: &str,
    key: T,
) -> Result<IdbKeyRange>
where
    T: SafeEncode,
{
    match store_cipher {
        Some(cipher) => key.encode_to_range_secure(table_name, cipher),
        None => key.encode_to_range(),
    }
    .map_err(|e| IndexeddbStateStoreError::StoreError(StoreError::Backend(anyhow!(e).into())))
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

        let meta_name = format!("{name}::{}", keys::INTERNAL_STATE);

        let (meta, store_cipher) = upgrade_meta_db(&meta_name, self.passphrase.as_deref()).await?;
        let inner =
            upgrade_inner_db(&name, store_cipher.as_deref(), migration_strategy, &meta).await?;

        Ok(IndexeddbStateStore { name, inner, meta, store_cipher })
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

    /// The version of the database containing the data.
    pub fn version(&self) -> u32 {
        self.inner.version() as u32
    }

    /// The version of the database containing the metadata.
    pub fn meta_version(&self) -> u32 {
        self.meta.version() as u32
    }

    /// Whether this database has any migration backups
    pub async fn has_backups(&self) -> Result<bool> {
        Ok(self
            .meta
            .transaction_on_one_with_mode(keys::BACKUPS_META, IdbTransactionMode::Readonly)?
            .object_store(keys::BACKUPS_META)?
            .count()?
            .await?
            > 0)
    }

    /// What's the database name of the latest backup<
    pub async fn latest_backup(&self) -> Result<Option<String>> {
        Ok(self
            .meta
            .transaction_on_one_with_mode(keys::BACKUPS_META, IdbTransactionMode::Readonly)?
            .object_store(keys::BACKUPS_META)?
            .open_cursor_with_direction(indexed_db_futures::prelude::IdbCursorDirection::Prev)?
            .await?
            .and_then(|c| c.value().as_string()))
    }

    fn serialize_event(&self, event: &impl Serialize) -> Result<JsValue> {
        serialize_event(self.store_cipher.as_deref(), event)
    }

    fn deserialize_event<T: DeserializeOwned>(&self, event: &JsValue) -> Result<T> {
        deserialize_event(self.store_cipher.as_deref(), event)
    }

    fn encode_key<T>(&self, table_name: &str, key: T) -> JsValue
    where
        T: SafeEncode,
    {
        encode_key(self.store_cipher.as_deref(), table_name, key)
    }

    fn encode_to_range<T>(&self, table_name: &str, key: T) -> Result<IdbKeyRange>
    where
        T: SafeEncode,
    {
        encode_to_range(self.store_cipher.as_deref(), table_name, key)
    }

    /// Get user IDs for the given room with the given memberships and stripped
    /// state.
    pub async fn get_user_ids_inner(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
        stripped: bool,
    ) -> Result<Vec<OwnedUserId>> {
        let store_name = if stripped { keys::STRIPPED_USER_IDS } else { keys::USER_IDS };

        let tx =
            self.inner.transaction_on_one_with_mode(store_name, IdbTransactionMode::Readonly)?;
        let store = tx.object_store(store_name)?;
        let range = self.encode_to_range(store_name, room_id)?;

        let user_ids = if memberships.is_empty() {
            // It should be faster to just get all user IDs in this case.
            store
                .get_all_with_key(&range)?
                .await?
                .iter()
                .filter_map(|f| self.deserialize_event::<RoomMember>(&f).ok().map(|m| m.user_id))
                .collect::<Vec<_>>()
        } else {
            let mut user_ids = Vec::new();
            let cursor = store.open_cursor_with_range(&range)?.await?;

            if let Some(cursor) = cursor {
                loop {
                    let value = cursor.value();
                    let member = self.deserialize_event::<RoomMember>(&value)?;

                    if memberships.matches(&member.membership) {
                        user_ids.push(member.user_id);
                    }

                    if !cursor.continue_cursor()?.await? {
                        break;
                    }
                }
            }

            user_ids
        };

        Ok(user_ids)
    }

    async fn get_custom_value_for_js(&self, jskey: &JsValue) -> Result<Option<Vec<u8>>> {
        self.inner
            .transaction_on_one_with_mode(keys::CUSTOM, IdbTransactionMode::Readonly)?
            .object_store(keys::CUSTOM)?
            .get(jskey)?
            .await?
            .map(|f| self.deserialize_event(&f))
            .transpose()
    }

    fn encode_kv_data_key(&self, key: StateStoreDataKey<'_>) -> JsValue {
        // Use the key (prefix) for the table name as well, to keep encoded
        // keys compatible for the sync token and filters, which were in
        // separate tables initially.
        match key {
            StateStoreDataKey::SyncToken => {
                self.encode_key(StateStoreDataKey::SYNC_TOKEN, StateStoreDataKey::SYNC_TOKEN)
            }
            StateStoreDataKey::Filter(filter_name) => {
                self.encode_key(StateStoreDataKey::FILTER, (StateStoreDataKey::FILTER, filter_name))
            }
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                self.encode_key(keys::KV, (StateStoreDataKey::USER_AVATAR_URL, user_id))
            }
        }
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
    ({ $($body:tt)* }) => {
        #[async_trait(?Send)]
        impl StateStore for IndexeddbStateStore {
            type Error = IndexeddbStateStoreError;

            $($body)*
        }
    };
}

#[cfg(not(target_arch = "wasm32"))]
macro_rules! impl_state_store {
    ({ $($body:tt)* }) => {
        impl IndexeddbStateStore {
            $($body)*
        }
    };
}

impl_state_store!({
    async fn get_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<Option<StateStoreDataValue>> {
        let encoded_key = self.encode_kv_data_key(key);

        let value = self
            .inner
            .transaction_on_one_with_mode(keys::KV, IdbTransactionMode::Readonly)?
            .object_store(keys::KV)?
            .get(&encoded_key)?
            .await?
            .map(|f| self.deserialize_event::<String>(&f))
            .transpose()?;

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

        let tx =
            self.inner.transaction_on_one_with_mode(keys::KV, IdbTransactionMode::Readwrite)?;

        let obj = tx.object_store(keys::KV)?;

        obj.put_key_val(&encoded_key, &self.serialize_event(&value)?)?;

        tx.await.into_result()?;

        Ok(())
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<()> {
        let encoded_key = self.encode_kv_data_key(key);

        let tx =
            self.inner.transaction_on_one_with_mode(keys::KV, IdbTransactionMode::Readwrite)?;
        let obj = tx.object_store(keys::KV)?;

        obj.delete(&encoded_key)?;

        tx.await.into_result()?;

        Ok(())
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let mut stores: HashSet<&'static str> = [
            (changes.sync_token.is_some(), keys::KV),
            (!changes.ambiguity_maps.is_empty(), keys::DISPLAY_NAMES),
            (!changes.account_data.is_empty(), keys::ACCOUNT_DATA),
            (!changes.presence.is_empty(), keys::PRESENCE),
            (!changes.profiles.is_empty(), keys::PROFILES),
            (!changes.room_account_data.is_empty(), keys::ROOM_ACCOUNT_DATA),
            (!changes.receipts.is_empty(), keys::ROOM_EVENT_RECEIPTS),
        ]
        .iter()
        .filter_map(|(id, key)| if *id { Some(*key) } else { None })
        .collect();

        if !changes.state.is_empty() {
            stores.extend([
                keys::ROOM_STATE,
                keys::USER_IDS,
                keys::STRIPPED_USER_IDS,
                keys::STRIPPED_ROOM_STATE,
                keys::PROFILES,
            ]);
        }

        if !changes.redactions.is_empty() {
            stores.extend([keys::ROOM_STATE, keys::ROOM_INFOS]);
        }

        if !changes.room_infos.is_empty() {
            stores.insert(keys::ROOM_INFOS);
        }

        if !changes.stripped_state.is_empty() {
            stores.extend([keys::STRIPPED_ROOM_STATE, keys::STRIPPED_USER_IDS]);
        }

        if !changes.receipts.is_empty() {
            stores.extend([keys::ROOM_EVENT_RECEIPTS, keys::ROOM_USER_RECEIPTS])
        }

        if stores.is_empty() {
            // nothing to do, quit early
            return Ok(());
        }

        let stores: Vec<&'static str> = stores.into_iter().collect();
        let tx =
            self.inner.transaction_on_multi_with_mode(&stores, IdbTransactionMode::Readwrite)?;

        if let Some(s) = &changes.sync_token {
            tx.object_store(keys::KV)?.put_key_val(
                &self.encode_kv_data_key(StateStoreDataKey::SyncToken),
                &self.serialize_event(s)?,
            )?;
        }

        if !changes.ambiguity_maps.is_empty() {
            let store = tx.object_store(keys::DISPLAY_NAMES)?;
            for (room_id, ambiguity_maps) in &changes.ambiguity_maps {
                for (display_name, map) in ambiguity_maps {
                    let key = self.encode_key(keys::DISPLAY_NAMES, (room_id, display_name));

                    store.put_key_val(&key, &self.serialize_event(&map)?)?;
                }
            }
        }

        if !changes.account_data.is_empty() {
            let store = tx.object_store(keys::ACCOUNT_DATA)?;
            for (event_type, event) in &changes.account_data {
                store.put_key_val(
                    &self.encode_key(keys::ACCOUNT_DATA, event_type),
                    &self.serialize_event(&event)?,
                )?;
            }
        }

        if !changes.room_account_data.is_empty() {
            let store = tx.object_store(keys::ROOM_ACCOUNT_DATA)?;
            for (room, events) in &changes.room_account_data {
                for (event_type, event) in events {
                    let key = self.encode_key(keys::ROOM_ACCOUNT_DATA, (room, event_type));
                    store.put_key_val(&key, &self.serialize_event(&event)?)?;
                }
            }
        }

        if !changes.state.is_empty() {
            let state = tx.object_store(keys::ROOM_STATE)?;
            let profiles = tx.object_store(keys::PROFILES)?;
            let user_ids = tx.object_store(keys::USER_IDS)?;
            let stripped_state = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
            let stripped_user_ids = tx.object_store(keys::STRIPPED_USER_IDS)?;

            for (room, event_types) in &changes.state {
                let profile_changes = changes.profiles.get(room);

                for (event_type, events) in event_types {
                    for (state_key, raw_event) in events {
                        let key = self.encode_key(keys::ROOM_STATE, (room, event_type, state_key));
                        state.put_key_val(&key, &self.serialize_event(&raw_event)?)?;
                        stripped_state.delete(&key)?;

                        if *event_type == StateEventType::RoomMember {
                            let event = match raw_event.deserialize_as::<SyncRoomMemberEvent>() {
                                Ok(ev) => ev,
                                Err(e) => {
                                    let event_id: Option<String> =
                                        raw_event.get_field("event_id").ok().flatten();
                                    debug!(event_id, "Failed to deserialize member event: {e}");
                                    continue;
                                }
                            };

                            let key = (room, state_key);

                            stripped_user_ids
                                .delete(&self.encode_key(keys::STRIPPED_USER_IDS, key))?;

                            user_ids.put_key_val_owned(
                                &self.encode_key(keys::USER_IDS, key),
                                &self.serialize_event(&RoomMember::from(&event))?,
                            )?;

                            if let Some(profile) =
                                profile_changes.and_then(|p| p.get(event.state_key()))
                            {
                                profiles.put_key_val_owned(
                                    &self.encode_key(keys::PROFILES, key),
                                    &self.serialize_event(&profile)?,
                                )?;
                            }
                        }
                    }
                }
            }
        }

        if !changes.room_infos.is_empty() {
            let room_infos = tx.object_store(keys::ROOM_INFOS)?;
            for (room_id, room_info) in &changes.room_infos {
                room_infos.put_key_val(
                    &self.encode_key(keys::ROOM_INFOS, room_id),
                    &self.serialize_event(&room_info)?,
                )?;
            }
        }

        if !changes.presence.is_empty() {
            let store = tx.object_store(keys::PRESENCE)?;
            for (sender, event) in &changes.presence {
                store.put_key_val(
                    &self.encode_key(keys::PRESENCE, sender),
                    &self.serialize_event(&event)?,
                )?;
            }
        }

        if !changes.stripped_state.is_empty() {
            let store = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
            let user_ids = tx.object_store(keys::STRIPPED_USER_IDS)?;

            for (room, event_types) in &changes.stripped_state {
                for (event_type, events) in event_types {
                    for (state_key, raw_event) in events {
                        let key = self
                            .encode_key(keys::STRIPPED_ROOM_STATE, (room, event_type, state_key));
                        store.put_key_val(&key, &self.serialize_event(&raw_event)?)?;

                        if *event_type == StateEventType::RoomMember {
                            let event = match raw_event.deserialize_as::<StrippedRoomMemberEvent>()
                            {
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

                            let key = (room, state_key);

                            user_ids.put_key_val_owned(
                                &self.encode_key(keys::STRIPPED_USER_IDS, key),
                                &self.serialize_event(&RoomMember::from(&event))?,
                            )?;
                        }
                    }
                }
            }
        }

        if !changes.receipts.is_empty() {
            let room_user_receipts = tx.object_store(keys::ROOM_USER_RECEIPTS)?;
            let room_event_receipts = tx.object_store(keys::ROOM_EVENT_RECEIPTS)?;

            for (room, content) in &changes.receipts {
                for (event_id, receipts) in &content.0 {
                    for (receipt_type, receipts) in receipts {
                        for (user_id, receipt) in receipts {
                            let key = match receipt.thread.as_str() {
                                Some(thread_id) => self.encode_key(
                                    keys::ROOM_USER_RECEIPTS,
                                    (room, receipt_type, thread_id, user_id),
                                ),
                                None => self.encode_key(
                                    keys::ROOM_USER_RECEIPTS,
                                    (room, receipt_type, user_id),
                                ),
                            };

                            if let Some((old_event, _)) =
                                room_user_receipts.get(&key)?.await?.and_then(|f| {
                                    self.deserialize_event::<(OwnedEventId, Receipt)>(&f).ok()
                                })
                            {
                                let key = match receipt.thread.as_str() {
                                    Some(thread_id) => self.encode_key(
                                        keys::ROOM_EVENT_RECEIPTS,
                                        (room, receipt_type, thread_id, old_event, user_id),
                                    ),
                                    None => self.encode_key(
                                        keys::ROOM_EVENT_RECEIPTS,
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
                                    keys::ROOM_EVENT_RECEIPTS,
                                    (room, receipt_type, thread_id, event_id, user_id),
                                ),
                                None => self.encode_key(
                                    keys::ROOM_EVENT_RECEIPTS,
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
            let state = tx.object_store(keys::ROOM_STATE)?;
            let room_info = tx.object_store(keys::ROOM_INFOS)?;

            for (room_id, redactions) in &changes.redactions {
                let range = self.encode_to_range(keys::ROOM_STATE, room_id)?;
                let Some(cursor) = state.open_cursor_with_range(&range)?.await? else { continue };

                let mut room_version = None;

                while let Some(key) = cursor.key() {
                    let raw_evt =
                        self.deserialize_event::<Raw<AnySyncStateEvent>>(&cursor.value())?;
                    if let Ok(Some(event_id)) = raw_evt.get_field::<OwnedEventId>("event_id") {
                        if let Some(redaction) = redactions.get(&event_id) {
                            let version = {
                                if room_version.is_none() {
                                    room_version.replace(room_info
                                        .get(&self.encode_key(keys::ROOM_INFOS, room_id))?
                                        .await?
                                        .and_then(|f| self.deserialize_event::<RoomInfo>(&f).ok())
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
            .transaction_on_one_with_mode(keys::PRESENCE, IdbTransactionMode::Readonly)?
            .object_store(keys::PRESENCE)?
            .get(&self.encode_key(keys::PRESENCE, user_id))?
            .await?
            .map(|f| self.deserialize_event(&f))
            .transpose()
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
        let stripped_range =
            self.encode_to_range(keys::STRIPPED_ROOM_STATE, (room_id, &event_type))?;
        let stripped_events = self
            .inner
            .transaction_on_one_with_mode(keys::STRIPPED_ROOM_STATE, IdbTransactionMode::Readonly)?
            .object_store(keys::STRIPPED_ROOM_STATE)?
            .get_all_with_key(&stripped_range)?
            .await?
            .iter()
            .filter_map(|f| {
                self.deserialize_event(&f).ok().map(RawAnySyncOrStrippedState::Stripped)
            })
            .collect::<Vec<_>>();

        if !stripped_events.is_empty() {
            return Ok(stripped_events);
        }

        let range = self.encode_to_range(keys::ROOM_STATE, (room_id, event_type))?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::ROOM_STATE, IdbTransactionMode::Readonly)?
            .object_store(keys::ROOM_STATE)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event(&f).ok().map(RawAnySyncOrStrippedState::Sync))
            .collect::<Vec<_>>())
    }

    async fn get_state_events_for_keys(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        if state_keys.is_empty() {
            return Ok(Vec::new());
        }

        let mut events = Vec::with_capacity(state_keys.len());

        {
            let txn = self.inner.transaction_on_one_with_mode(
                keys::STRIPPED_ROOM_STATE,
                IdbTransactionMode::Readonly,
            )?;
            let store = txn.object_store(keys::STRIPPED_ROOM_STATE)?;

            for state_key in state_keys {
                if let Some(event) =
                    store
                        .get(&self.encode_key(
                            keys::STRIPPED_ROOM_STATE,
                            (room_id, &event_type, state_key),
                        ))?
                        .await?
                        .map(|f| self.deserialize_event(&f))
                        .transpose()?
                {
                    events.push(RawAnySyncOrStrippedState::Stripped(event));
                }
            }

            if !events.is_empty() {
                return Ok(events);
            }
        }

        let txn = self
            .inner
            .transaction_on_one_with_mode(keys::ROOM_STATE, IdbTransactionMode::Readonly)?;
        let store = txn.object_store(keys::ROOM_STATE)?;

        for state_key in state_keys {
            if let Some(event) = store
                .get(&self.encode_key(keys::ROOM_STATE, (room_id, &event_type, state_key)))?
                .await?
                .map(|f| self.deserialize_event(&f))
                .transpose()?
            {
                events.push(RawAnySyncOrStrippedState::Sync(event));
            }
        }

        Ok(events)
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalStateEvent<RoomMemberEventContent>>> {
        self.inner
            .transaction_on_one_with_mode(keys::PROFILES, IdbTransactionMode::Readonly)?
            .object_store(keys::PROFILES)?
            .get(&self.encode_key(keys::PROFILES, (room_id, user_id)))?
            .await?
            .map(|f| self.deserialize_event(&f))
            .transpose()
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        let entries: Vec<_> = self
            .inner
            .transaction_on_one_with_mode(keys::ROOM_INFOS, IdbTransactionMode::Readonly)?
            .object_store(keys::ROOM_INFOS)?
            .get_all()?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event::<RoomInfo>(&f).ok())
            .collect();

        Ok(entries)
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        let txn = self
            .inner
            .transaction_on_one_with_mode(keys::ROOM_INFOS, IdbTransactionMode::Readonly)?;
        let store = txn.object_store(keys::ROOM_INFOS)?;

        let mut infos = Vec::new();
        let cursor = store.open_cursor()?.await?;

        if let Some(cursor) = cursor {
            loop {
                let value = cursor.value();
                let info = self.deserialize_event::<RoomInfo>(&value)?;

                if info.state() == RoomState::Invited {
                    infos.push(info);
                }

                if !cursor.continue_cursor()?.await? {
                    break;
                }
            }
        }

        Ok(infos)
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>> {
        self.inner
            .transaction_on_one_with_mode(keys::DISPLAY_NAMES, IdbTransactionMode::Readonly)?
            .object_store(keys::DISPLAY_NAMES)?
            .get(&self.encode_key(keys::DISPLAY_NAMES, (room_id, display_name)))?
            .await?
            .map(|f| self.deserialize_event::<BTreeSet<OwnedUserId>>(&f))
            .unwrap_or_else(|| Ok(Default::default()))
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.inner
            .transaction_on_one_with_mode(keys::ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(keys::ACCOUNT_DATA)?
            .get(&self.encode_key(keys::ACCOUNT_DATA, event_type))?
            .await?
            .map(|f| self.deserialize_event(&f))
            .transpose()
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.inner
            .transaction_on_one_with_mode(keys::ROOM_ACCOUNT_DATA, IdbTransactionMode::Readonly)?
            .object_store(keys::ROOM_ACCOUNT_DATA)?
            .get(&self.encode_key(keys::ROOM_ACCOUNT_DATA, (room_id, event_type)))?
            .await?
            .map(|f| self.deserialize_event(&f))
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
                .encode_key(keys::ROOM_USER_RECEIPTS, (room_id, receipt_type, thread_id, user_id)),
            None => self.encode_key(keys::ROOM_USER_RECEIPTS, (room_id, receipt_type, user_id)),
        };
        self.inner
            .transaction_on_one_with_mode(keys::ROOM_USER_RECEIPTS, IdbTransactionMode::Readonly)?
            .object_store(keys::ROOM_USER_RECEIPTS)?
            .get(&key)?
            .await?
            .map(|f| self.deserialize_event(&f))
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
                keys::ROOM_EVENT_RECEIPTS,
                (room_id, receipt_type, thread_id, event_id),
            ),
            None => {
                self.encode_to_range(keys::ROOM_EVENT_RECEIPTS, (room_id, receipt_type, event_id))
            }
        }?;
        let tx = self.inner.transaction_on_one_with_mode(
            keys::ROOM_EVENT_RECEIPTS,
            IdbTransactionMode::Readonly,
        )?;
        let store = tx.object_store(keys::ROOM_EVENT_RECEIPTS)?;

        Ok(store
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|f| self.deserialize_event(&f).ok())
            .collect::<Vec<_>>())
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        let key = self
            .encode_key(keys::MEDIA, (request.source.unique_key(), request.format.unique_key()));
        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;

        tx.object_store(keys::MEDIA)?.put_key_val(&key, &self.serialize_event(&data)?)?;

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let key = self
            .encode_key(keys::MEDIA, (request.source.unique_key(), request.format.unique_key()));
        self.inner
            .transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readonly)?
            .object_store(keys::MEDIA)?
            .get(&key)?
            .await?
            .map(|f| self.deserialize_event(&f))
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
            self.inner.transaction_on_one_with_mode(keys::CUSTOM, IdbTransactionMode::Readwrite)?;

        tx.object_store(keys::CUSTOM)?.put_key_val(&jskey, &self.serialize_event(&value)?)?;

        tx.await.into_result().map_err(IndexeddbStateStoreError::from)?;
        Ok(prev)
    }

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let jskey = JsValue::from_str(core::str::from_utf8(key).map_err(StoreError::Codec)?);

        let prev = self.get_custom_value_for_js(&jskey).await?;

        let tx =
            self.inner.transaction_on_one_with_mode(keys::CUSTOM, IdbTransactionMode::Readwrite)?;

        tx.object_store(keys::CUSTOM)?.delete(&jskey)?;

        tx.await.into_result().map_err(IndexeddbStateStoreError::from)?;
        Ok(prev)
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        let key = self
            .encode_key(keys::MEDIA, (request.source.unique_key(), request.format.unique_key()));
        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;

        tx.object_store(keys::MEDIA)?.delete(&key)?;

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let range = self.encode_to_range(keys::MEDIA, uri)?;
        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(keys::MEDIA)?;

        for k in store.get_all_keys_with_key(&range)?.await?.iter() {
            store.delete(&k)?;
        }

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        let direct_stores = [keys::ROOM_INFOS];

        let prefixed_stores = [
            keys::PROFILES,
            keys::DISPLAY_NAMES,
            keys::USER_IDS,
            keys::ROOM_STATE,
            keys::ROOM_ACCOUNT_DATA,
            keys::ROOM_EVENT_RECEIPTS,
            keys::ROOM_USER_RECEIPTS,
            keys::STRIPPED_ROOM_STATE,
            keys::STRIPPED_USER_IDS,
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

    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>> {
        let ids = self.get_user_ids_inner(room_id, memberships, true).await?;
        if !ids.is_empty() {
            return Ok(ids);
        }
        self.get_user_ids_inner(room_id, memberships, false).await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        self.get_user_ids(room_id, RoomMemberships::INVITE).await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        self.get_user_ids(room_id, RoomMemberships::JOIN).await
    }
});

/// A room member.
#[derive(Debug, Serialize, Deserialize)]
struct RoomMember {
    user_id: OwnedUserId,
    membership: MembershipState,
}

impl From<&SyncStateEvent<RoomMemberEventContent>> for RoomMember {
    fn from(event: &SyncStateEvent<RoomMemberEventContent>) -> Self {
        Self { user_id: event.state_key().clone(), membership: event.membership().clone() }
    }
}

impl From<&StrippedRoomMemberEvent> for RoomMember {
    fn from(event: &StrippedRoomMemberEvent) -> Self {
        Self { user_id: event.state_key.clone(), membership: event.content.membership.clone() }
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
