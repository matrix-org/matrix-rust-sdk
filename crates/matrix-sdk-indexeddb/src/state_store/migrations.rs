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
    collections::{HashMap, HashSet},
    sync::Arc,
};

use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::{prelude::*, request::OpenDbRequest, IdbDatabase, IdbVersionChangeEvent};
use js_sys::Date as JsDate;
use matrix_sdk_base::{RoomInfo, StateStoreDataKey};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    events::{
        room::member::{StrippedRoomMemberEvent, SyncRoomMemberEvent},
        StateEventType,
    },
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
use wasm_bindgen::JsValue;
use web_sys::IdbTransactionMode;

use super::{
    deserialize_event, encode_key, encode_to_range, keys, serialize_event, Result, RoomMember,
    ALL_STORES,
};
use crate::IndexeddbStateStoreError;

const CURRENT_DB_VERSION: u32 = 7;
const CURRENT_META_DB_VERSION: u32 = 2;

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

#[derive(Clone, Serialize, Deserialize)]
struct StoreKeyWrapper(Vec<u8>);

mod old_keys {
    pub const SESSION: &str = "session";
    pub const SYNC_TOKEN: &str = "sync_token";
    pub const MEMBERS: &str = "members";
    pub const STRIPPED_MEMBERS: &str = "stripped_members";
    pub const JOINED_USER_IDS: &str = "joined_user_ids";
    pub const INVITED_USER_IDS: &str = "invited_user_ids";
    pub const STRIPPED_JOINED_USER_IDS: &str = "stripped_joined_user_ids";
    pub const STRIPPED_INVITED_USER_IDS: &str = "stripped_invited_user_ids";
    pub const STRIPPED_ROOM_INFOS: &str = "stripped_room_infos";
}

pub async fn upgrade_meta_db(
    meta_name: &str,
    passphrase: Option<&str>,
) -> Result<(IdbDatabase, Option<Arc<StoreCipher>>)> {
    // Meta database.
    let mut db_req: OpenDbRequest = IdbDatabase::open_u32(meta_name, CURRENT_META_DB_VERSION)?;
    db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        let db = evt.db();
        let old_version = evt.old_version() as u32;

        if old_version < 1 {
            db.create_object_store(keys::INTERNAL_STATE)?;
        }

        if old_version < 2 {
            db.create_object_store(keys::BACKUPS_META)?;
        }

        Ok(())
    }));

    let meta_db: IdbDatabase = db_req.into_future().await?;

    let store_cipher = if let Some(passphrase) = passphrase {
        let tx: IdbTransaction<'_> = meta_db
            .transaction_on_one_with_mode(keys::INTERNAL_STATE, IdbTransactionMode::Readwrite)?;
        let ob = tx.object_store(keys::INTERNAL_STATE)?;

        let cipher = if let Some(StoreKeyWrapper(inner)) = ob
            .get(&JsValue::from_str(keys::STORE_KEY))?
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
                &JsValue::from_str(keys::STORE_KEY),
                &JsValue::from_serde(&StoreKeyWrapper(export))?,
            )?;
            cipher
        };

        tx.await.into_result()?;
        Some(Arc::new(cipher))
    } else {
        None
    };

    Ok((meta_db, store_cipher))
}

/// Helper struct for upgrading the inner DB.
#[derive(Debug, Clone, Default)]
pub struct OngoingMigration {
    /// Names of stores to drop.
    drop_stores: HashSet<&'static str>,
    /// Names of stores to create.
    create_stores: HashSet<&'static str>,
    /// Store name => key-value data to add.
    data: HashMap<&'static str, Vec<(JsValue, JsValue)>>,
}

impl OngoingMigration {
    /// Merge this migration with the given one.
    fn merge(&mut self, other: OngoingMigration) {
        self.drop_stores.extend(other.drop_stores);
        self.create_stores.extend(other.create_stores);

        for (store, data) in other.data {
            let entry = self.data.entry(store).or_default();
            entry.extend(data);
        }
    }
}

pub async fn upgrade_inner_db(
    name: &str,
    store_cipher: Option<&StoreCipher>,
    migration_strategy: MigrationConflictStrategy,
    meta_db: &IdbDatabase,
) -> Result<IdbDatabase> {
    let mut migration = OngoingMigration::default();
    {
        // This is a hack, we need to open the database a first time to get the current
        // version.
        // The indexed_db_futures crate doesn't let us access the transaction so we
        // can't migrate data inside the `onupgradeneeded` callback. Instead we see if
        // we need to migrate some data before the upgrade, then let the store process
        // the upgrade.
        // See <https://github.com/Alorel/rust-indexed-db/issues/20>
        let has_store_cipher = store_cipher.is_some();
        let pre_db = IdbDatabase::open(name)?.into_future().await?;

        // Even if the web-sys bindings expose the version as a f64, the IndexedDB API
        // works with an unsigned integer.
        // See <https://github.com/rustwasm/wasm-bindgen/issues/1149>
        let mut old_version = pre_db.version() as u32;

        // Inside the `onupgradeneeded` callback we would know whether it's a new DB
        // because the old version would be set to 0, here it is already set to 1 so we
        // check if the stores exist.
        if old_version == 1 && pre_db.object_store_names().next().is_none() {
            old_version = 0;
        }

        // Upgrades to v1 and v2 (re)create empty stores, while the other upgrades
        // change data that is already in the stores, so we use exclusive branches here.
        if old_version == 0 {
            migration.create_stores.extend(ALL_STORES);
        } else if old_version < 2 && has_store_cipher {
            match migration_strategy {
                MigrationConflictStrategy::BackupAndDrop => {
                    backup_v1(&pre_db, meta_db).await?;
                    migration.drop_stores.extend(V1_STORES);
                    migration.create_stores.extend(ALL_STORES);
                }
                MigrationConflictStrategy::Drop => {
                    migration.drop_stores.extend(V1_STORES);
                    migration.create_stores.extend(ALL_STORES);
                }
                MigrationConflictStrategy::Raise => {
                    return Err(IndexeddbStateStoreError::MigrationConflict {
                        name: name.to_owned(),
                        old_version,
                        new_version: CURRENT_DB_VERSION,
                    });
                }
            }
        } else {
            if old_version < 3 {
                migrate_to_v3(&pre_db, store_cipher).await?;
            }
            if old_version < 4 {
                migration.merge(migrate_to_v4(&pre_db, store_cipher).await?);
            }
            if old_version < 5 {
                migration.merge(migrate_to_v5(&pre_db, store_cipher).await?);
            }
            if old_version < 6 {
                migration.merge(migrate_to_v6(&pre_db, store_cipher).await?);
            }
            if old_version < 7 {
                migration.merge(migrate_to_v7(&pre_db, store_cipher).await?);
            }
        }

        pre_db.close();
    }

    let mut db_req: OpenDbRequest = IdbDatabase::open_u32(name, CURRENT_DB_VERSION)?;
    db_req.set_on_upgrade_needed(Some(move |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        // Changing the format can only happen in the upgrade procedure
        for store in &migration.drop_stores {
            evt.db().delete_object_store(store)?;
        }
        for store in &migration.create_stores {
            evt.db().create_object_store(store)?;
        }

        Ok(())
    }));

    let db = db_req.into_future().await?;

    // Finally, we can add data to the newly created tables if needed.
    if !migration.data.is_empty() {
        let stores: Vec<_> = migration.data.keys().copied().collect();
        let tx = db.transaction_on_multi_with_mode(&stores, IdbTransactionMode::Readwrite)?;

        for (name, data) in migration.data {
            let store = tx.object_store(name)?;
            for (key, value) in data {
                store.put_key_val(&key, &value)?;
            }
        }

        tx.await.into_result()?;
    }

    Ok(db)
}

pub const V1_STORES: &[&str] = &[
    old_keys::SESSION,
    keys::ACCOUNT_DATA,
    old_keys::MEMBERS,
    keys::PROFILES,
    keys::DISPLAY_NAMES,
    old_keys::JOINED_USER_IDS,
    old_keys::INVITED_USER_IDS,
    keys::ROOM_STATE,
    keys::ROOM_INFOS,
    keys::PRESENCE,
    keys::ROOM_ACCOUNT_DATA,
    old_keys::STRIPPED_ROOM_INFOS,
    old_keys::STRIPPED_MEMBERS,
    keys::STRIPPED_ROOM_STATE,
    old_keys::STRIPPED_JOINED_USER_IDS,
    old_keys::STRIPPED_INVITED_USER_IDS,
    keys::ROOM_USER_RECEIPTS,
    keys::ROOM_EVENT_RECEIPTS,
    keys::MEDIA,
    keys::CUSTOM,
    old_keys::SYNC_TOKEN,
];

async fn backup_v1(source: &IdbDatabase, meta: &IdbDatabase) -> Result<()> {
    let now = JsDate::now();
    let backup_name = format!("backup-{}-{now}", source.name());

    let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&backup_name, source.version())?;
    db_req.set_on_upgrade_needed(Some(move |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        // migrating to version 1
        let db = evt.db();
        for name in V1_STORES {
            db.create_object_store(name)?;
        }
        Ok(())
    }));
    let target = db_req.into_future().await?;

    for name in V1_STORES {
        let source_tx = source.transaction_on_one_with_mode(name, IdbTransactionMode::Readonly)?;
        let source_obj = source_tx.object_store(name)?;
        let Some(curs) = source_obj.open_cursor()?.await? else {
            continue;
        };

        let data = curs.into_vec(0).await?;

        let target_tx = target.transaction_on_one_with_mode(name, IdbTransactionMode::Readwrite)?;
        let target_obj = target_tx.object_store(name)?;

        for kv in data {
            target_obj.put_key_val(kv.key(), kv.value())?;
        }

        target_tx.await.into_result()?;
    }

    let tx =
        meta.transaction_on_one_with_mode(keys::BACKUPS_META, IdbTransactionMode::Readwrite)?;
    let backup_store = tx.object_store(keys::BACKUPS_META)?;
    backup_store.put_key_val(&JsValue::from_f64(now), &JsValue::from_str(&backup_name))?;

    tx.await;

    Ok(())
}

async fn v3_fix_store(
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
            let raw_json: Box<RawJsonValue> = deserialize_event(store_cipher, &cursor.value())?;

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

/// Fix serialized redacted state events.
async fn migrate_to_v3(db: &IdbDatabase, store_cipher: Option<&StoreCipher>) -> Result<()> {
    let tx = db.transaction_on_multi_with_mode(
        &[keys::ROOM_STATE, keys::ROOM_INFOS],
        IdbTransactionMode::Readwrite,
    )?;

    v3_fix_store(&tx.object_store(keys::ROOM_STATE)?, store_cipher).await?;
    v3_fix_store(&tx.object_store(keys::ROOM_INFOS)?, store_cipher).await?;

    tx.await.into_result().map_err(|e| e.into())
}

/// Move the content of the SYNC_TOKEN and SESSION stores to the new KV store.
async fn migrate_to_v4(
    db: &IdbDatabase,
    store_cipher: Option<&StoreCipher>,
) -> Result<OngoingMigration> {
    let tx = db.transaction_on_multi_with_mode(
        &[old_keys::SYNC_TOKEN, old_keys::SESSION],
        IdbTransactionMode::Readonly,
    )?;
    let mut values = Vec::new();

    // Sync token
    let sync_token_store = tx.object_store(old_keys::SYNC_TOKEN)?;
    let sync_token = sync_token_store.get(&JsValue::from_str(old_keys::SYNC_TOKEN))?.await?;

    if let Some(sync_token) = sync_token {
        values.push((
            encode_key(store_cipher, StateStoreDataKey::SYNC_TOKEN, StateStoreDataKey::SYNC_TOKEN),
            sync_token,
        ));
    }

    // Filters
    let session_store = tx.object_store(old_keys::SESSION)?;
    let range =
        encode_to_range(store_cipher, StateStoreDataKey::FILTER, StateStoreDataKey::FILTER)?;
    if let Some(cursor) = session_store.open_cursor_with_range(&range)?.await? {
        while let Some(key) = cursor.key() {
            let value = cursor.value();
            values.push((key, value));
            cursor.continue_cursor()?.await?;
        }
    }

    tx.await.into_result()?;

    let mut data = HashMap::new();
    if !values.is_empty() {
        data.insert(keys::KV, values);
    }

    Ok(OngoingMigration {
        drop_stores: [old_keys::SYNC_TOKEN, old_keys::SESSION].into_iter().collect(),
        create_stores: [keys::KV].into_iter().collect(),
        data,
    })
}

/// Move the member events with other state events.
async fn migrate_to_v5(
    db: &IdbDatabase,
    store_cipher: Option<&StoreCipher>,
) -> Result<OngoingMigration> {
    let tx = db.transaction_on_multi_with_mode(
        &[
            old_keys::MEMBERS,
            old_keys::STRIPPED_MEMBERS,
            keys::ROOM_STATE,
            keys::STRIPPED_ROOM_STATE,
            keys::ROOM_INFOS,
            old_keys::STRIPPED_ROOM_INFOS,
        ],
        IdbTransactionMode::Readwrite,
    )?;

    let members_store = tx.object_store(old_keys::MEMBERS)?;
    let state_store = tx.object_store(keys::ROOM_STATE)?;
    let room_infos = tx
        .object_store(keys::ROOM_INFOS)?
        .get_all()?
        .await?
        .iter()
        .filter_map(|f| deserialize_event::<RoomInfo>(store_cipher, &f).ok())
        .collect::<Vec<_>>();

    for room_info in room_infos {
        let room_id = room_info.room_id();
        let range = encode_to_range(store_cipher, old_keys::MEMBERS, room_id)?;
        for value in members_store.get_all_with_key(&range)?.await?.iter() {
            let raw_member_event =
                deserialize_event::<Raw<SyncRoomMemberEvent>>(store_cipher, &value)?;
            let state_key = raw_member_event.get_field::<String>("state_key")?.unwrap_or_default();
            let key = encode_key(
                store_cipher,
                keys::ROOM_STATE,
                (room_id, StateEventType::RoomMember, state_key),
            );

            state_store.add_key_val(&key, &value)?;
        }
    }

    let stripped_members_store = tx.object_store(old_keys::STRIPPED_MEMBERS)?;
    let stripped_state_store = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
    let stripped_room_infos = tx
        .object_store(old_keys::STRIPPED_ROOM_INFOS)?
        .get_all()?
        .await?
        .iter()
        .filter_map(|f| deserialize_event::<RoomInfo>(store_cipher, &f).ok())
        .collect::<Vec<_>>();

    for room_info in stripped_room_infos {
        let room_id = room_info.room_id();
        let range = encode_to_range(store_cipher, old_keys::STRIPPED_MEMBERS, room_id)?;
        for value in stripped_members_store.get_all_with_key(&range)?.await?.iter() {
            let raw_member_event =
                deserialize_event::<Raw<StrippedRoomMemberEvent>>(store_cipher, &value)?;
            let state_key = raw_member_event.get_field::<String>("state_key")?.unwrap_or_default();
            let key = encode_key(
                store_cipher,
                keys::STRIPPED_ROOM_STATE,
                (room_id, StateEventType::RoomMember, state_key),
            );

            stripped_state_store.add_key_val(&key, &value)?;
        }
    }

    tx.await.into_result()?;

    Ok(OngoingMigration {
        drop_stores: [old_keys::MEMBERS, old_keys::STRIPPED_MEMBERS].into_iter().collect(),
        create_stores: Default::default(),
        data: Default::default(),
    })
}

/// Remove the old user IDs stores and populate the new ones.
async fn migrate_to_v6(
    db: &IdbDatabase,
    store_cipher: Option<&StoreCipher>,
) -> Result<OngoingMigration> {
    // We only have joined and invited user IDs in the old store, so instead we will
    // use the room member events to populate the new store.
    let tx = db.transaction_on_multi_with_mode(
        &[
            keys::ROOM_STATE,
            keys::ROOM_INFOS,
            keys::STRIPPED_ROOM_STATE,
            old_keys::STRIPPED_ROOM_INFOS,
        ],
        IdbTransactionMode::Readonly,
    )?;

    let state_store = tx.object_store(keys::ROOM_STATE)?;
    let room_infos = tx
        .object_store(keys::ROOM_INFOS)?
        .get_all()?
        .await?
        .iter()
        .filter_map(|f| deserialize_event::<RoomInfo>(store_cipher, &f).ok())
        .collect::<Vec<_>>();
    let mut values = Vec::new();

    for room_info in room_infos {
        let room_id = room_info.room_id();
        let range =
            encode_to_range(store_cipher, keys::ROOM_STATE, (room_id, StateEventType::RoomMember))?;
        for value in state_store.get_all_with_key(&range)?.await?.iter() {
            let member_event = deserialize_event::<Raw<SyncRoomMemberEvent>>(store_cipher, &value)?
                .deserialize()?;
            let key = encode_key(store_cipher, keys::USER_IDS, (room_id, member_event.state_key()));
            let value = serialize_event(store_cipher, &RoomMember::from(&member_event))?;

            values.push((key, value));
        }
    }

    let stripped_state_store = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
    let stripped_room_infos = tx
        .object_store(old_keys::STRIPPED_ROOM_INFOS)?
        .get_all()?
        .await?
        .iter()
        .filter_map(|f| deserialize_event::<RoomInfo>(store_cipher, &f).ok())
        .collect::<Vec<_>>();
    let mut stripped_values = Vec::new();

    for room_info in stripped_room_infos {
        let room_id = room_info.room_id();
        let range = encode_to_range(
            store_cipher,
            keys::STRIPPED_ROOM_STATE,
            (room_id, StateEventType::RoomMember),
        )?;
        for value in stripped_state_store.get_all_with_key(&range)?.await?.iter() {
            let stripped_member_event =
                deserialize_event::<Raw<StrippedRoomMemberEvent>>(store_cipher, &value)?
                    .deserialize()?;
            let key = encode_key(
                store_cipher,
                keys::STRIPPED_USER_IDS,
                (room_id, &stripped_member_event.state_key),
            );
            let value = serialize_event(store_cipher, &RoomMember::from(&stripped_member_event))?;

            stripped_values.push((key, value));
        }
    }

    tx.await.into_result()?;

    let mut data = HashMap::new();
    if !values.is_empty() {
        data.insert(keys::USER_IDS, values);
    }
    if !stripped_values.is_empty() {
        data.insert(keys::STRIPPED_USER_IDS, stripped_values);
    }

    Ok(OngoingMigration {
        drop_stores: HashSet::from_iter([
            old_keys::JOINED_USER_IDS,
            old_keys::INVITED_USER_IDS,
            old_keys::STRIPPED_JOINED_USER_IDS,
            old_keys::STRIPPED_INVITED_USER_IDS,
        ]),
        create_stores: HashSet::from_iter([keys::USER_IDS, keys::STRIPPED_USER_IDS]),
        data,
    })
}

/// Remove the stripped room infos store and migrate the data with the other
/// room infos, as well as .
async fn migrate_to_v7(
    db: &IdbDatabase,
    store_cipher: Option<&StoreCipher>,
) -> Result<OngoingMigration> {
    let tx = db.transaction_on_multi_with_mode(
        &[old_keys::STRIPPED_ROOM_INFOS],
        IdbTransactionMode::Readonly,
    )?;

    let room_infos = tx
        .object_store(old_keys::STRIPPED_ROOM_INFOS)?
        .get_all()?
        .await?
        .iter()
        .filter_map(|value| {
            deserialize_event::<RoomInfo>(store_cipher, &value)
                .ok()
                .map(|info| (encode_key(store_cipher, keys::ROOM_INFOS, info.room_id()), value))
        })
        .collect::<Vec<_>>();

    tx.await.into_result()?;

    let mut data = HashMap::new();
    if !room_infos.is_empty() {
        data.insert(keys::ROOM_INFOS, room_infos);
    }

    Ok(OngoingMigration {
        drop_stores: HashSet::from_iter([old_keys::STRIPPED_ROOM_INFOS]),
        data,
        ..Default::default()
    })
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use assert_matches::assert_matches;
    use indexed_db_futures::prelude::*;
    use matrix_sdk_base::{
        deserialized_responses::RawMemberEvent, store::StateStoreExt, RoomInfo, RoomMemberships,
        RoomState, StateStore, StateStoreDataKey, StoreError,
    };
    use matrix_sdk_test::{async_test, test_json};
    use ruma::{
        events::{
            room::member::{StrippedRoomMemberEvent, SyncRoomMemberEvent},
            AnySyncStateEvent, StateEventType,
        },
        room_id,
        serde::Raw,
        user_id,
    };
    use serde_json::json;
    use uuid::Uuid;
    use wasm_bindgen::JsValue;

    use super::{old_keys, MigrationConflictStrategy, CURRENT_DB_VERSION, CURRENT_META_DB_VERSION};
    use crate::{
        safe_encode::SafeEncode,
        state_store::{encode_key, keys, serialize_event, Result},
        IndexeddbStateStore, IndexeddbStateStoreError,
    };

    const CUSTOM_DATA_KEY: &[u8] = b"custom_data_key";
    const CUSTOM_DATA: &[u8] = b"some_custom_data";

    pub async fn create_fake_db(name: &str, version: u32) -> Result<IdbDatabase> {
        let mut db_req: OpenDbRequest = IdbDatabase::open_u32(name, version)?;
        db_req.set_on_upgrade_needed(Some(
            move |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
                let db = evt.db();

                // Stores common to all versions.
                let common_stores = &[
                    keys::ACCOUNT_DATA,
                    keys::PROFILES,
                    keys::DISPLAY_NAMES,
                    keys::ROOM_STATE,
                    keys::ROOM_INFOS,
                    keys::PRESENCE,
                    keys::ROOM_ACCOUNT_DATA,
                    keys::STRIPPED_ROOM_STATE,
                    keys::ROOM_USER_RECEIPTS,
                    keys::ROOM_EVENT_RECEIPTS,
                    keys::MEDIA,
                    keys::CUSTOM,
                ];

                for name in common_stores {
                    db.create_object_store(name)?;
                }

                if version < 4 {
                    for name in [old_keys::SYNC_TOKEN, old_keys::SESSION] {
                        db.create_object_store(name)?;
                    }
                }
                if version >= 4 {
                    db.create_object_store(keys::KV)?;
                }
                if version < 5 {
                    for name in [old_keys::MEMBERS, old_keys::STRIPPED_MEMBERS] {
                        db.create_object_store(name)?;
                    }
                }
                if version < 6 {
                    for name in [
                        old_keys::INVITED_USER_IDS,
                        old_keys::JOINED_USER_IDS,
                        old_keys::STRIPPED_INVITED_USER_IDS,
                        old_keys::STRIPPED_JOINED_USER_IDS,
                    ] {
                        db.create_object_store(name)?;
                    }
                }
                if version >= 6 {
                    for name in [keys::USER_IDS, keys::STRIPPED_USER_IDS] {
                        db.create_object_store(name)?;
                    }
                }
                if version < 7 {
                    db.create_object_store(old_keys::STRIPPED_ROOM_INFOS)?;
                }

                Ok(())
            },
        ));
        db_req.into_future().await.map_err(Into::into)
    }

    #[async_test]
    pub async fn test_new_store() -> Result<()> {
        let name = format!("new-store-no-cipher-{}", Uuid::new_v4().as_hyphenated().to_string());

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;
        // this didn't create any backup
        assert_eq!(store.has_backups().await?, false);
        // simple check that the layout exists.
        assert_eq!(store.get_custom_value(CUSTOM_DATA_KEY).await?, None);

        // Check versions.
        assert_eq!(store.version(), CURRENT_DB_VERSION);
        assert_eq!(store.meta_version(), CURRENT_META_DB_VERSION);

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_v1_to_v2_plain() -> Result<()> {
        let name = format!("migrating-v2-no-cipher-{}", Uuid::new_v4().as_hyphenated().to_string());

        // Create and populate db.
        {
            let db = create_fake_db(&name, 1).await?;
            let tx =
                db.transaction_on_one_with_mode(keys::CUSTOM, IdbTransactionMode::Readwrite)?;
            let custom = tx.object_store(keys::CUSTOM)?;
            let jskey = JsValue::from_str(
                core::str::from_utf8(CUSTOM_DATA_KEY).map_err(StoreError::Codec)?,
            );
            custom.put_key_val(&jskey, &serialize_event(None, &CUSTOM_DATA)?)?;
            tx.await.into_result()?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;
        // this didn't create any backup
        assert_eq!(store.has_backups().await?, false);
        // Custom data is still there.
        let stored_data = assert_matches!(
            store.get_custom_value(CUSTOM_DATA_KEY).await?,
            Some(d) => d
        );
        assert_eq!(stored_data, CUSTOM_DATA);

        // Check versions.
        assert_eq!(store.version(), CURRENT_DB_VERSION);
        assert_eq!(store.meta_version(), CURRENT_META_DB_VERSION);

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_v1_to_v2_with_pw() -> Result<()> {
        let name =
            format!("migrating-v2-with-cipher-{}", Uuid::new_v4().as_hyphenated().to_string());
        let passphrase = "somepassphrase".to_owned();

        // Create and populate db.
        {
            let db = create_fake_db(&name, 1).await?;
            let tx =
                db.transaction_on_one_with_mode(keys::CUSTOM, IdbTransactionMode::Readwrite)?;
            let custom = tx.object_store(keys::CUSTOM)?;
            let jskey = JsValue::from_str(
                core::str::from_utf8(CUSTOM_DATA_KEY).map_err(StoreError::Codec)?,
            );
            custom.put_key_val(&jskey, &serialize_event(None, &CUSTOM_DATA)?)?;
            tx.await.into_result()?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store =
            IndexeddbStateStore::builder().name(name).passphrase(passphrase).build().await?;
        // this creates a backup by default
        assert_eq!(store.has_backups().await?, true);
        assert!(store.latest_backup().await?.is_some(), "No backup_found");
        // the data is gone
        assert_eq!(store.get_custom_value(CUSTOM_DATA_KEY).await?, None);

        // Check versions.
        assert_eq!(store.version(), CURRENT_DB_VERSION);
        assert_eq!(store.meta_version(), CURRENT_META_DB_VERSION);

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_v1_to_v2_with_pw_drops() -> Result<()> {
        let name = format!(
            "migrating-v2-with-cipher-drops-{}",
            Uuid::new_v4().as_hyphenated().to_string()
        );
        let passphrase = "some-other-passphrase".to_owned();

        // Create and populate db.
        {
            let db = create_fake_db(&name, 1).await?;
            let tx =
                db.transaction_on_one_with_mode(keys::CUSTOM, IdbTransactionMode::Readwrite)?;
            let custom = tx.object_store(keys::CUSTOM)?;
            let jskey = JsValue::from_str(
                core::str::from_utf8(CUSTOM_DATA_KEY).map_err(StoreError::Codec)?,
            );
            custom.put_key_val(&jskey, &serialize_event(None, &CUSTOM_DATA)?)?;
            tx.await.into_result()?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder()
            .name(name)
            .passphrase(passphrase)
            .migration_conflict_strategy(MigrationConflictStrategy::Drop)
            .build()
            .await?;
        // this doesn't create a backup
        assert_eq!(store.has_backups().await?, false);
        // the data is gone
        assert_eq!(store.get_custom_value(CUSTOM_DATA_KEY).await?, None);

        // Check versions.
        assert_eq!(store.version(), CURRENT_DB_VERSION);
        assert_eq!(store.meta_version(), CURRENT_META_DB_VERSION);

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_v1_to_v2_with_pw_raise() -> Result<()> {
        let name = format!(
            "migrating-v2-with-cipher-raises-{}",
            Uuid::new_v4().as_hyphenated().to_string()
        );
        let passphrase = "some-other-passphrase".to_owned();

        // Create and populate db.
        {
            let db = create_fake_db(&name, 1).await?;
            let tx =
                db.transaction_on_one_with_mode(keys::CUSTOM, IdbTransactionMode::Readwrite)?;
            let custom = tx.object_store(keys::CUSTOM)?;
            let jskey = JsValue::from_str(
                core::str::from_utf8(CUSTOM_DATA_KEY).map_err(StoreError::Codec)?,
            );
            custom.put_key_val(&jskey, &serialize_event(None, &CUSTOM_DATA)?)?;
            tx.await.into_result()?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store_res = IndexeddbStateStore::builder()
            .name(name)
            .passphrase(passphrase)
            .migration_conflict_strategy(MigrationConflictStrategy::Raise)
            .build()
            .await;

        assert_matches!(store_res, Err(IndexeddbStateStoreError::MigrationConflict { .. }));

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_to_v3() -> Result<()> {
        let name = format!("migrating-v3-{}", Uuid::new_v4().as_hyphenated().to_string());

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
            let db = create_fake_db(&name, 2).await?;
            let tx =
                db.transaction_on_one_with_mode(keys::ROOM_STATE, IdbTransactionMode::Readwrite)?;
            let state = tx.object_store(keys::ROOM_STATE)?;
            let key: JsValue = (room_id, StateEventType::RoomTopic, "").as_encoded_string().into();
            state.put_key_val(&key, &serialize_event(None, &wrong_redacted_state_event)?)?;
            tx.await.into_result()?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;
        let event =
            store.get_state_event(room_id, StateEventType::RoomTopic, "").await.unwrap().unwrap();
        event.deserialize().unwrap();

        // Check versions.
        assert_eq!(store.version(), CURRENT_DB_VERSION);
        assert_eq!(store.meta_version(), CURRENT_META_DB_VERSION);

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_to_v4() -> Result<()> {
        let name = format!("migrating-v4-{}", Uuid::new_v4().as_hyphenated().to_string());

        let sync_token = "a_very_unique_string";
        let filter_1 = "filter_1";
        let filter_1_id = "filter_1_id";
        let filter_2 = "filter_2";
        let filter_2_id = "filter_2_id";

        // Populate DB with old table.
        {
            let db = create_fake_db(&name, 3).await?;
            let tx = db.transaction_on_multi_with_mode(
                &[old_keys::SYNC_TOKEN, old_keys::SESSION],
                IdbTransactionMode::Readwrite,
            )?;

            let sync_token_store = tx.object_store(old_keys::SYNC_TOKEN)?;
            sync_token_store.put_key_val(
                &JsValue::from_str(old_keys::SYNC_TOKEN),
                &serialize_event(None, &sync_token)?,
            )?;

            let session_store = tx.object_store(old_keys::SESSION)?;
            session_store.put_key_val(
                &encode_key(None, StateStoreDataKey::FILTER, (StateStoreDataKey::FILTER, filter_1)),
                &serialize_event(None, &filter_1_id)?,
            )?;
            session_store.put_key_val(
                &encode_key(None, StateStoreDataKey::FILTER, (StateStoreDataKey::FILTER, filter_2)),
                &serialize_event(None, &filter_2_id)?,
            )?;

            tx.await.into_result()?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;

        let stored_sync_token = store
            .get_kv_data(StateStoreDataKey::SyncToken)
            .await?
            .unwrap()
            .into_sync_token()
            .unwrap();
        assert_eq!(stored_sync_token, sync_token);

        let stored_filter_1_id = store
            .get_kv_data(StateStoreDataKey::Filter(filter_1))
            .await?
            .unwrap()
            .into_filter()
            .unwrap();
        assert_eq!(stored_filter_1_id, filter_1_id);

        let stored_filter_2_id = store
            .get_kv_data(StateStoreDataKey::Filter(filter_2))
            .await?
            .unwrap()
            .into_filter()
            .unwrap();
        assert_eq!(stored_filter_2_id, filter_2_id);

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_to_v5() -> Result<()> {
        let name = format!("migrating-v5-{}", Uuid::new_v4().as_hyphenated().to_string());

        let room_id = room_id!("!room:localhost");
        let member_event =
            Raw::new(&*test_json::MEMBER_INVITE).unwrap().cast::<SyncRoomMemberEvent>();
        let user_id = user_id!("@invited:localhost");

        let stripped_room_id = room_id!("!stripped_room:localhost");
        let stripped_member_event =
            Raw::new(&*test_json::MEMBER_STRIPPED).unwrap().cast::<StrippedRoomMemberEvent>();
        let stripped_user_id = user_id!("@example:localhost");

        // Populate DB with old table.
        {
            let db = create_fake_db(&name, 4).await?;
            let tx = db.transaction_on_multi_with_mode(
                &[
                    old_keys::MEMBERS,
                    keys::ROOM_INFOS,
                    old_keys::STRIPPED_MEMBERS,
                    old_keys::STRIPPED_ROOM_INFOS,
                ],
                IdbTransactionMode::Readwrite,
            )?;

            let members_store = tx.object_store(old_keys::MEMBERS)?;
            members_store.put_key_val(
                &encode_key(None, old_keys::MEMBERS, (room_id, user_id)),
                &serialize_event(None, &member_event)?,
            )?;
            let room_infos_store = tx.object_store(keys::ROOM_INFOS)?;
            let room_info = RoomInfo::new(room_id, RoomState::Joined);
            room_infos_store.put_key_val(
                &encode_key(None, keys::ROOM_INFOS, room_id),
                &serialize_event(None, &room_info)?,
            )?;

            let stripped_members_store = tx.object_store(old_keys::STRIPPED_MEMBERS)?;
            stripped_members_store.put_key_val(
                &encode_key(None, old_keys::STRIPPED_MEMBERS, (stripped_room_id, stripped_user_id)),
                &serialize_event(None, &stripped_member_event)?,
            )?;
            let stripped_room_infos_store = tx.object_store(old_keys::STRIPPED_ROOM_INFOS)?;
            let stripped_room_info = RoomInfo::new(stripped_room_id, RoomState::Invited);
            stripped_room_infos_store.put_key_val(
                &encode_key(None, old_keys::STRIPPED_ROOM_INFOS, stripped_room_id),
                &serialize_event(None, &stripped_room_info)?,
            )?;

            tx.await.into_result()?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;

        let stored_member_event = assert_matches!(
            store.get_member_event(room_id, user_id).await,
            Ok(Some(RawMemberEvent::Sync(e))) => e
        );
        assert_eq!(stored_member_event.json().get(), member_event.json().get());

        let stored_stripped_member_event = assert_matches!(
            store.get_member_event(stripped_room_id, stripped_user_id).await,
            Ok(Some(RawMemberEvent::Stripped(e))) => e
        );
        assert_eq!(stored_stripped_member_event.json().get(), stripped_member_event.json().get());

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_to_v6() -> Result<()> {
        let name = format!("migrating-v6-{}", Uuid::new_v4().as_hyphenated().to_string());

        let room_id = room_id!("!room:localhost");
        let invite_member_event =
            Raw::new(&*test_json::MEMBER_INVITE).unwrap().cast::<SyncRoomMemberEvent>();
        let invite_user_id = user_id!("@invited:localhost");
        let ban_member_event =
            Raw::new(&*test_json::MEMBER_BAN).unwrap().cast::<SyncRoomMemberEvent>();
        let ban_user_id = user_id!("@banned:localhost");

        let stripped_room_id = room_id!("!stripped_room:localhost");
        let stripped_member_event =
            Raw::new(&*test_json::MEMBER_STRIPPED).unwrap().cast::<StrippedRoomMemberEvent>();
        let stripped_user_id = user_id!("@example:localhost");

        // Populate DB with old table.
        {
            let db = create_fake_db(&name, 5).await?;
            let tx = db.transaction_on_multi_with_mode(
                &[
                    keys::ROOM_STATE,
                    keys::ROOM_INFOS,
                    keys::STRIPPED_ROOM_STATE,
                    old_keys::STRIPPED_ROOM_INFOS,
                    old_keys::INVITED_USER_IDS,
                    old_keys::JOINED_USER_IDS,
                    old_keys::STRIPPED_INVITED_USER_IDS,
                    old_keys::STRIPPED_JOINED_USER_IDS,
                ],
                IdbTransactionMode::Readwrite,
            )?;

            let state_store = tx.object_store(keys::ROOM_STATE)?;
            state_store.put_key_val(
                &encode_key(
                    None,
                    keys::ROOM_STATE,
                    (room_id, StateEventType::RoomMember, invite_user_id),
                ),
                &serialize_event(None, &invite_member_event)?,
            )?;
            state_store.put_key_val(
                &encode_key(
                    None,
                    keys::ROOM_STATE,
                    (room_id, StateEventType::RoomMember, ban_user_id),
                ),
                &serialize_event(None, &ban_member_event)?,
            )?;
            let room_infos_store = tx.object_store(keys::ROOM_INFOS)?;
            let room_info = RoomInfo::new(room_id, RoomState::Joined);
            room_infos_store.put_key_val(
                &encode_key(None, keys::ROOM_INFOS, room_id),
                &serialize_event(None, &room_info)?,
            )?;

            let stripped_state_store = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
            stripped_state_store.put_key_val(
                &encode_key(
                    None,
                    keys::STRIPPED_ROOM_STATE,
                    (stripped_room_id, StateEventType::RoomMember, stripped_user_id),
                ),
                &serialize_event(None, &stripped_member_event)?,
            )?;
            let stripped_room_infos_store = tx.object_store(old_keys::STRIPPED_ROOM_INFOS)?;
            let stripped_room_info = RoomInfo::new(stripped_room_id, RoomState::Invited);
            stripped_room_infos_store.put_key_val(
                &encode_key(None, old_keys::STRIPPED_ROOM_INFOS, stripped_room_id),
                &serialize_event(None, &stripped_room_info)?,
            )?;

            // Populate the old user IDs stores to check the data is not reused.
            let joined_user_id = user_id!("@joined_user:localhost");
            tx.object_store(old_keys::JOINED_USER_IDS)?.put_key_val(
                &encode_key(None, old_keys::JOINED_USER_IDS, (room_id, joined_user_id)),
                &serialize_event(None, &joined_user_id)?,
            )?;
            let invited_user_id = user_id!("@invited_user:localhost");
            tx.object_store(old_keys::INVITED_USER_IDS)?.put_key_val(
                &encode_key(None, old_keys::INVITED_USER_IDS, (room_id, invited_user_id)),
                &serialize_event(None, &invited_user_id)?,
            )?;
            let stripped_joined_user_id = user_id!("@stripped_joined_user:localhost");
            tx.object_store(old_keys::STRIPPED_JOINED_USER_IDS)?.put_key_val(
                &encode_key(
                    None,
                    old_keys::STRIPPED_JOINED_USER_IDS,
                    (room_id, stripped_joined_user_id),
                ),
                &serialize_event(None, &stripped_joined_user_id)?,
            )?;
            let stripped_invited_user_id = user_id!("@stripped_invited_user:localhost");
            tx.object_store(old_keys::STRIPPED_INVITED_USER_IDS)?.put_key_val(
                &encode_key(
                    None,
                    old_keys::STRIPPED_INVITED_USER_IDS,
                    (room_id, stripped_invited_user_id),
                ),
                &serialize_event(None, &stripped_invited_user_id)?,
            )?;

            tx.await.into_result()?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;

        assert_eq!(store.get_user_ids(room_id, RoomMemberships::JOIN).await.unwrap().len(), 0);
        assert_eq!(
            store.get_user_ids(room_id, RoomMemberships::INVITE).await.unwrap().as_slice(),
            [invite_user_id.to_owned()]
        );
        let user_ids = store.get_user_ids(room_id, RoomMemberships::empty()).await.unwrap();
        assert_eq!(user_ids.len(), 2);
        assert!(user_ids.contains(&invite_user_id.to_owned()));
        assert!(user_ids.contains(&ban_user_id.to_owned()));

        assert_eq!(
            store.get_user_ids(stripped_room_id, RoomMemberships::JOIN).await.unwrap().as_slice(),
            [stripped_user_id.to_owned()]
        );
        assert_eq!(
            store.get_user_ids(stripped_room_id, RoomMemberships::INVITE).await.unwrap().len(),
            0
        );
        assert_eq!(
            store
                .get_user_ids(stripped_room_id, RoomMemberships::empty())
                .await
                .unwrap()
                .as_slice(),
            [stripped_user_id.to_owned()]
        );

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_to_v7() -> Result<()> {
        let name = format!("migrating-v7-{}", Uuid::new_v4().as_hyphenated().to_string());

        let room_id = room_id!("!room:localhost");
        let stripped_room_id = room_id!("!stripped_room:localhost");

        // Populate DB with old table.
        {
            let db = create_fake_db(&name, 6).await?;
            let tx = db.transaction_on_multi_with_mode(
                &[keys::ROOM_INFOS, old_keys::STRIPPED_ROOM_INFOS],
                IdbTransactionMode::Readwrite,
            )?;

            let room_infos_store = tx.object_store(keys::ROOM_INFOS)?;
            let room_info = RoomInfo::new(room_id, RoomState::Joined);
            room_infos_store.put_key_val(
                &encode_key(None, keys::ROOM_INFOS, room_id),
                &serialize_event(None, &room_info)?,
            )?;

            let stripped_room_infos_store = tx.object_store(old_keys::STRIPPED_ROOM_INFOS)?;
            let stripped_room_info = RoomInfo::new(stripped_room_id, RoomState::Invited);
            stripped_room_infos_store.put_key_val(
                &encode_key(None, old_keys::STRIPPED_ROOM_INFOS, stripped_room_id),
                &serialize_event(None, &stripped_room_info)?,
            )?;

            tx.await.into_result()?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;

        assert_eq!(store.get_room_infos().await.unwrap().len(), 2);

        Ok(())
    }
}
