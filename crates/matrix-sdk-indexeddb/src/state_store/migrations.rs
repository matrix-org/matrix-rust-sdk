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
use matrix_sdk_base::StateStoreDataKey;
use matrix_sdk_store_encryption::StoreCipher;
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
use wasm_bindgen::JsValue;
use web_sys::IdbTransactionMode;

use super::{
    deserialize_event, encode_key, encode_to_range, serialize_event, Result, ALL_STORES, KEYS,
};
use crate::IndexeddbStateStoreError;

const CURRENT_DB_VERSION: u32 = 4;
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

#[allow(non_snake_case)]
mod OLD_KEYS {
    // Old stores
    pub const SESSION: &str = "session";
    pub const SYNC_TOKEN: &str = "sync_token";
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
            db.create_object_store(KEYS::INTERNAL_STATE)?;
        }

        if old_version < 2 {
            db.create_object_store(KEYS::BACKUPS_META)?;
        }

        Ok(())
    }));

    let meta_db: IdbDatabase = db_req.into_future().await?;

    let store_cipher = if let Some(passphrase) = passphrase {
        let tx: IdbTransaction<'_> = meta_db
            .transaction_on_one_with_mode(KEYS::INTERNAL_STATE, IdbTransactionMode::Readwrite)?;
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

    Ok((meta_db, store_cipher))
}

// Helper struct for upgrading the inner DB.
#[derive(Debug, Clone, Default)]
pub struct OngoingMigration {
    // Names of stores to drop.
    drop_stores: HashSet<&'static str>,
    // Names of stores to create.
    create_stores: HashSet<&'static str>,
    // Store name => key-value data to add.
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
    OLD_KEYS::SESSION,
    KEYS::ACCOUNT_DATA,
    KEYS::MEMBERS,
    KEYS::PROFILES,
    KEYS::DISPLAY_NAMES,
    KEYS::JOINED_USER_IDS,
    KEYS::INVITED_USER_IDS,
    KEYS::ROOM_STATE,
    KEYS::ROOM_INFOS,
    KEYS::PRESENCE,
    KEYS::ROOM_ACCOUNT_DATA,
    KEYS::STRIPPED_ROOM_INFOS,
    KEYS::STRIPPED_MEMBERS,
    KEYS::STRIPPED_ROOM_STATE,
    KEYS::STRIPPED_JOINED_USER_IDS,
    KEYS::STRIPPED_INVITED_USER_IDS,
    KEYS::ROOM_USER_RECEIPTS,
    KEYS::ROOM_EVENT_RECEIPTS,
    KEYS::MEDIA,
    KEYS::CUSTOM,
    OLD_KEYS::SYNC_TOKEN,
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
        let Some(curs) = source_obj
            .open_cursor()?
            .await? else {
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
        meta.transaction_on_one_with_mode(KEYS::BACKUPS_META, IdbTransactionMode::Readwrite)?;
    let backup_store = tx.object_store(KEYS::BACKUPS_META)?;
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

/// Fix serialized redacted state events.
async fn migrate_to_v3(db: &IdbDatabase, store_cipher: Option<&StoreCipher>) -> Result<()> {
    let tx = db.transaction_on_multi_with_mode(
        &[KEYS::ROOM_STATE, KEYS::ROOM_INFOS],
        IdbTransactionMode::Readwrite,
    )?;

    v3_fix_store(&tx.object_store(KEYS::ROOM_STATE)?, store_cipher).await?;
    v3_fix_store(&tx.object_store(KEYS::ROOM_INFOS)?, store_cipher).await?;

    tx.await.into_result().map_err(|e| e.into())
}

/// Move the content of the SYNC_TOKEN and SESSION stores to the new KV store.
async fn migrate_to_v4(
    db: &IdbDatabase,
    store_cipher: Option<&StoreCipher>,
) -> Result<OngoingMigration> {
    let tx = db.transaction_on_multi_with_mode(
        &[OLD_KEYS::SYNC_TOKEN, OLD_KEYS::SESSION],
        IdbTransactionMode::Readonly,
    )?;
    let mut values = Vec::new();

    // Sync token
    let sync_token_store = tx.object_store(OLD_KEYS::SYNC_TOKEN)?;
    let sync_token = sync_token_store.get(&JsValue::from_str(OLD_KEYS::SYNC_TOKEN))?.await?;

    if let Some(sync_token) = sync_token {
        values.push((
            encode_key(
                store_cipher,
                StateStoreDataKey::SyncToken.encoding_key(),
                StateStoreDataKey::SyncToken.encoding_key(),
            ),
            sync_token,
        ));
    }

    // Filters
    let session_store = tx.object_store(OLD_KEYS::SESSION)?;
    let range = encode_to_range(
        store_cipher,
        StateStoreDataKey::Filter("").encoding_key(),
        StateStoreDataKey::Filter("").encoding_key(),
    )?;
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
        data.insert(KEYS::KV, values);
    }

    Ok(OngoingMigration {
        drop_stores: [OLD_KEYS::SYNC_TOKEN, OLD_KEYS::SESSION].into_iter().collect(),
        create_stores: [KEYS::KV].into_iter().collect(),
        data,
    })
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use assert_matches::assert_matches;
    use indexed_db_futures::prelude::*;
    use matrix_sdk_base::{StateStore, StateStoreDataKey, StoreError};
    use matrix_sdk_test::async_test;
    use ruma::{
        events::{AnySyncStateEvent, StateEventType},
        room_id,
    };
    use serde_json::json;
    use uuid::Uuid;
    use wasm_bindgen::JsValue;

    use super::{
        MigrationConflictStrategy, CURRENT_DB_VERSION, CURRENT_META_DB_VERSION, OLD_KEYS, V1_STORES,
    };
    use crate::{
        safe_encode::SafeEncode,
        state_store::{encode_key, serialize_event, Result, ALL_STORES, KEYS},
        IndexeddbStateStore, IndexeddbStateStoreError,
    };

    const CUSTOM_DATA_KEY: &[u8] = b"custom_data_key";
    const CUSTOM_DATA: &[u8] = b"some_custom_data";

    pub async fn create_fake_db(name: &str, version: u32) -> Result<IdbDatabase> {
        let mut db_req: OpenDbRequest = IdbDatabase::open_u32(name, version)?;
        db_req.set_on_upgrade_needed(Some(
            move |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
                let db = evt.db();

                // Initialize stores.
                if version < 4 {
                    for name in V1_STORES {
                        db.create_object_store(name)?;
                    }
                } else {
                    for name in ALL_STORES {
                        db.create_object_store(name)?;
                    }
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
                db.transaction_on_one_with_mode(KEYS::CUSTOM, IdbTransactionMode::Readwrite)?;
            let custom = tx.object_store(KEYS::CUSTOM)?;
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
                db.transaction_on_one_with_mode(KEYS::CUSTOM, IdbTransactionMode::Readwrite)?;
            let custom = tx.object_store(KEYS::CUSTOM)?;
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
                db.transaction_on_one_with_mode(KEYS::CUSTOM, IdbTransactionMode::Readwrite)?;
            let custom = tx.object_store(KEYS::CUSTOM)?;
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
                db.transaction_on_one_with_mode(KEYS::CUSTOM, IdbTransactionMode::Readwrite)?;
            let custom = tx.object_store(KEYS::CUSTOM)?;
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
                db.transaction_on_one_with_mode(KEYS::ROOM_STATE, IdbTransactionMode::Readwrite)?;
            let state = tx.object_store(KEYS::ROOM_STATE)?;
            let key = (room_id, StateEventType::RoomTopic, "").encode();
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
                &[OLD_KEYS::SYNC_TOKEN, OLD_KEYS::SESSION],
                IdbTransactionMode::Readwrite,
            )?;

            let sync_token_store = tx.object_store(OLD_KEYS::SYNC_TOKEN)?;
            sync_token_store.put_key_val(
                &JsValue::from_str(OLD_KEYS::SYNC_TOKEN),
                &serialize_event(None, &sync_token)?,
            )?;

            let session_store = tx.object_store(OLD_KEYS::SESSION)?;
            session_store.put_key_val(
                &encode_key(
                    None,
                    StateStoreDataKey::Filter("").encoding_key(),
                    (StateStoreDataKey::Filter("").encoding_key(), filter_1),
                ),
                &serialize_event(None, &filter_1_id)?,
            )?;
            session_store.put_key_val(
                &encode_key(
                    None,
                    StateStoreDataKey::Filter("").encoding_key(),
                    (StateStoreDataKey::Filter("").encoding_key(), filter_2),
                ),
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
}
