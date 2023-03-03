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

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::{prelude::*, request::OpenDbRequest, IdbDatabase, IdbVersionChangeEvent};
use js_sys::Date as JsDate;
use matrix_sdk_store_encryption::StoreCipher;
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
use wasm_bindgen::JsValue;
use web_sys::IdbTransactionMode;

use super::{deserialize_event, serialize_event, Result, ALL_STORES, KEYS};
use crate::IndexeddbStateStoreError;

const CURRENT_DB_VERSION: f64 = 1.2;
const CURRENT_META_DB_VERSION: f64 = 2.0;

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

pub async fn upgrade_meta_db(
    meta_name: &str,
    passphrase: Option<&str>,
) -> Result<(IdbDatabase, Option<Arc<StoreCipher>>)> {
    // Meta database.
    let mut db_req: OpenDbRequest = IdbDatabase::open_f64(meta_name, CURRENT_META_DB_VERSION)?;
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

pub async fn upgrade_inner_db(
    name: &str,
    store_cipher: Option<&StoreCipher>,
    migration_strategy: MigrationConflictStrategy,
    meta_db: &IdbDatabase,
) -> Result<IdbDatabase> {
    let mut recreate_stores = false;
    {
        // checkup up in a separate call, whether we have to backup or do anything else
        // to the db. Unfortunately the set_on_upgrade_needed doesn't allow async fn
        // which we need to execute the backup.
        let has_store_cipher = store_cipher.is_some();
        let mut db_req: OpenDbRequest = IdbDatabase::open_f64(name, 1.0)?;
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
                    backup(&pre_db, meta_db).await?;
                    recreate_stores = true;
                }
                MigrationConflictStrategy::Drop => {
                    recreate_stores = true;
                }
                MigrationConflictStrategy::Raise => {
                    return Err(IndexeddbStateStoreError::MigrationConflict {
                        name: name.to_owned(),
                        old_version,
                        new_version: CURRENT_DB_VERSION,
                    })
                }
            }
        } else if old_version < 1.2 {
            migrate_to_v1_2(&pre_db, store_cipher).await?;
        } else {
            // Nothing to be done
        }
    }

    let mut db_req: OpenDbRequest = IdbDatabase::open_f64(name, CURRENT_DB_VERSION)?;
    db_req.set_on_upgrade_needed(Some(move |evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        // changing the format can only happen in the upgrade procedure
        if recreate_stores {
            drop_stores(evt.db())?;
            create_stores(evt.db())?;
        }
        Ok(())
    }));

    Ok(db_req.into_future().await?)
}

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

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
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

    use super::{serialize_event, MigrationConflictStrategy, Result, ALL_STORES, KEYS};
    use crate::{safe_encode::SafeEncode, IndexeddbStateStore, IndexeddbStateStoreError};

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
