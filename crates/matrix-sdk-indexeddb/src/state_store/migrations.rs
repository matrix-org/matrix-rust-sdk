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
use indexed_db_futures::{
    database::{Database, VersionChangeEvent},
    error::Error,
    future::OpenDbRequest,
    object_store::ObjectStore,
    prelude::*,
    transaction::{Transaction, TransactionMode},
};
use js_sys::Date as JsDate;
use matrix_sdk_base::{
    StateStoreDataKey, deserialized_responses::SyncOrStrippedState,
    store::migration_helpers::RoomInfoV1,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    events::{
        StateEventType,
        room::{
            create::RoomCreateEventContent,
            member::{StrippedRoomMemberEvent, SyncRoomMemberEvent},
        },
    },
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue as RawJsonValue, Value as JsonValue};
use wasm_bindgen::JsValue;

use super::{
    ALL_STORES, Result, RoomMember, deserialize_value, encode_key, encode_to_range, keys,
    serialize_value,
};
use crate::IndexeddbStateStoreError;

const CURRENT_DB_VERSION: u32 = 14;
const CURRENT_META_DB_VERSION: u32 = 2;

/// Sometimes Migrations can't proceed without having to drop existing
/// data. This allows you to configure, how these cases should be handled.
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
    pub const MEDIA: &str = "media";
}

pub async fn upgrade_meta_db(
    meta_name: &str,
    passphrase: Option<&str>,
) -> Result<(Database, Option<Arc<StoreCipher>>)> {
    // Meta database.
    let db_req: OpenDbRequest = Database::open(meta_name)
        .with_version(CURRENT_META_DB_VERSION)
        .with_on_upgrade_needed(
            |evt: VersionChangeEvent, tx: &Transaction<'_>| -> Result<(), Error> {
                let db = tx.db();
                let old_version = evt.old_version() as u32;

                if old_version < 1 {
                    db.create_object_store(keys::INTERNAL_STATE).build()?;
                }

                if old_version < 2 {
                    db.create_object_store(keys::BACKUPS_META).build()?;
                }

                Ok(())
            },
        )
        .build()?;

    let meta_db: Database = db_req.await?;

    let store_cipher = if let Some(passphrase) = passphrase {
        let tx: Transaction<'_> = meta_db
            .transaction(keys::INTERNAL_STATE)
            .with_mode(TransactionMode::Readwrite)
            .build()?;
        let ob = tx.object_store(keys::INTERNAL_STATE)?;

        let cipher = if let Some(StoreKeyWrapper(inner)) = ob
            .get(&JsValue::from_str(keys::STORE_KEY))
            .await?
            .map(|v: JsValue| v.into_serde())
            .transpose()?
        {
            StoreCipher::import(passphrase, &inner)?
        } else {
            let cipher = StoreCipher::new()?;
            #[cfg(not(test))]
            let export = cipher.export(passphrase)?;
            #[cfg(test)]
            let export = cipher._insecure_export_fast_for_testing(passphrase)?;
            ob.put(&StoreKeyWrapper(export)).with_key(keys::STORE_KEY.to_owned()).serde()?.await?;
            cipher
        };

        tx.commit().await?;
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

pub async fn upgrade_inner_db(
    name: &str,
    store_cipher: Option<&StoreCipher>,
    migration_strategy: MigrationConflictStrategy,
    meta_db: &Database,
) -> Result<Database> {
    let mut db = Database::open(name).await?;

    // Even if the web-sys bindings expose the version as a f64, the IndexedDB API
    // works with an unsigned integer.
    // See <https://github.com/rustwasm/wasm-bindgen/issues/1149>
    let mut old_version = db.version() as u32;

    if old_version < CURRENT_DB_VERSION {
        // This is a hack, we need to open the database a first time to get the current
        // version.
        // The indexed_db_futures crate doesn't let us access the transaction so we
        // can't migrate data inside the `onupgradeneeded` callback. Instead we see if
        // we need to migrate some data before the upgrade, then let the store process
        // the upgrade.
        // See <https://github.com/Alorel/rust-indexed-db/issues/20>
        let has_store_cipher = store_cipher.is_some();

        // Inside the `onupgradeneeded` callback we would know whether it's a new DB
        // because the old version would be set to 0, here it is already set to 1 so we
        // check if the stores exist.
        if old_version == 1 && db.object_store_names().next().is_none() {
            old_version = 0;
        }

        // Upgrades to v1 and v2 (re)create empty stores, while the other upgrades
        // change data that is already in the stores, so we use exclusive branches here.
        if old_version == 0 {
            let migration = OngoingMigration {
                create_stores: ALL_STORES.iter().copied().collect(),
                ..Default::default()
            };
            db = apply_migration(db, CURRENT_DB_VERSION, migration).await?;
        } else if old_version < 2 && has_store_cipher {
            match migration_strategy {
                MigrationConflictStrategy::BackupAndDrop => {
                    backup_v1(&db, meta_db).await?;
                }
                MigrationConflictStrategy::Drop => {}
                MigrationConflictStrategy::Raise => {
                    return Err(IndexeddbStateStoreError::MigrationConflict {
                        name: name.to_owned(),
                        old_version,
                        new_version: CURRENT_DB_VERSION,
                    });
                }
            }

            let migration = OngoingMigration {
                drop_stores: V1_STORES.iter().copied().collect(),
                create_stores: ALL_STORES.iter().copied().collect(),
                ..Default::default()
            };
            db = apply_migration(db, CURRENT_DB_VERSION, migration).await?;
        } else {
            if old_version < 3 {
                db = migrate_to_v3(db, store_cipher).await?;
            }
            if old_version < 4 {
                db = migrate_to_v4(db, store_cipher).await?;
            }
            if old_version < 5 {
                db = migrate_to_v5(db, store_cipher).await?;
            }
            if old_version < 6 {
                db = migrate_to_v6(db, store_cipher).await?;
            }
            if old_version < 7 {
                db = migrate_to_v7(db, store_cipher).await?;
            }
            if old_version < 8 {
                db = migrate_to_v8(db, store_cipher).await?;
            }
            if old_version < 9 {
                db = migrate_to_v9(db).await?;
            }
            if old_version < 10 {
                db = migrate_to_v10(db).await?;
            }
            if old_version < 11 {
                db = migrate_to_v11(db).await?;
            }
            if old_version < 12 {
                db = migrate_to_v12(db).await?;
            }
            if old_version < 13 {
                db = migrate_to_v13(db).await?;
            }
            if old_version < 14 {
                db = migrate_to_v14(db).await?;
            }
        }

        db.close();

        let db_req: OpenDbRequest = Database::open(name)
            .with_version(CURRENT_DB_VERSION)
            .with_on_upgrade_needed(
                move |evt: VersionChangeEvent, _: &Transaction<'_>| -> Result<(), Error> {
                    // Sanity check.
                    // There should be no upgrade needed since the database should have already been
                    // upgraded to the latest version.
                    panic!(
                        "Opening database that was not fully upgraded: \
                     DB version: {}; latest version: {CURRENT_DB_VERSION}",
                        evt.old_version()
                    )
                },
            )
            .build()?;
        db = db_req.await?;
    }

    Ok(db)
}

/// Apply the given migration by upgrading the database with the given name to
/// the given version.
async fn apply_migration(
    db: Database,
    version: u32,
    migration: OngoingMigration,
) -> Result<Database> {
    let name = db.name();
    db.close();

    let db_req: OpenDbRequest = Database::open(&name)
        .with_version(version)
        .with_on_upgrade_needed(
            move |_: VersionChangeEvent, tx: &Transaction<'_>| -> Result<(), Error> {
                // Changing the format can only happen in the upgrade procedure
                for store in &migration.drop_stores {
                    tx.db().delete_object_store(store)?;
                }
                for store in &migration.create_stores {
                    tx.db().create_object_store(store).build()?;
                }

                Ok(())
            },
        )
        .build()?;

    let db = db_req.await?;

    // Finally, we can add data to the newly created tables if needed.
    if !migration.data.is_empty() {
        let stores: Vec<_> = migration.data.keys().copied().collect();
        let tx = db.transaction(stores).with_mode(TransactionMode::Readwrite).build()?;

        for (name, data) in migration.data {
            let store = tx.object_store(name)?;
            for (key, value) in data {
                store.put(&value).with_key(key).await?;
            }
        }

        tx.commit().await?;
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
    old_keys::MEDIA,
    keys::CUSTOM,
    old_keys::SYNC_TOKEN,
];

async fn backup_v1(source: &Database, meta: &Database) -> Result<()> {
    let now = JsDate::now();
    let backup_name = format!("backup-{}-{now}", source.name());

    let db_req: OpenDbRequest = Database::open(&backup_name)
        .with_version(source.version())
        .with_on_upgrade_needed(
            move |_: VersionChangeEvent, tx: &Transaction<'_>| -> Result<(), Error> {
                // migrating to version 1
                let db = tx.db();
                for name in V1_STORES {
                    db.create_object_store(name).build()?;
                }
                Ok(())
            },
        )
        .build()?;
    let target = db_req.await?;

    for name in V1_STORES {
        let source_tx = source.transaction(*name).with_mode(TransactionMode::Readonly).build()?;
        let source_obj = source_tx.object_store(name)?;
        let Some(mut curs) = source_obj.open_cursor().await? else {
            continue;
        };

        let mut data = vec![];
        while let Some(value) = curs.next_record::<JsValue>().await? {
            if let Some(key) = curs.key::<JsValue>()? {
                data.push((key, value));
            }
        }

        let target_tx = target.transaction(*name).with_mode(TransactionMode::Readwrite).build()?;
        let target_obj = target_tx.object_store(name)?;

        for (key, value) in data {
            target_obj.put(value).with_key(key).await?;
        }

        target_tx.commit().await?;
    }

    let tx = meta.transaction(keys::BACKUPS_META).with_mode(TransactionMode::Readwrite).build()?;
    let backup_store = tx.object_store(keys::BACKUPS_META)?;
    backup_store.put(&backup_name).with_key(now).await?;

    tx.commit().await?;

    target.close();

    Ok(())
}

async fn v3_fix_store(store: &ObjectStore<'_>, store_cipher: Option<&StoreCipher>) -> Result<()> {
    fn maybe_fix_json(raw_json: &RawJsonValue) -> Result<Option<JsonValue>> {
        let json = raw_json.get();

        if json.contains(r#""content":null"#) {
            let mut value: JsonValue = serde_json::from_str(json)?;
            if let Some(content) = value.get_mut("content")
                && matches!(content, JsonValue::Null)
            {
                *content = JsonValue::Object(Default::default());
                return Ok(Some(value));
            }
        }

        Ok(None)
    }

    let cursor = store.open_cursor().await?;

    if let Some(mut cursor) = cursor {
        while let Some(value) = cursor.next_record().await? {
            let raw_json: Box<RawJsonValue> = deserialize_value(store_cipher, &value)?;

            if let Some(fixed_json) = maybe_fix_json(&raw_json)? {
                cursor.update(&serialize_value(store_cipher, &fixed_json)?).await?;
            }
        }
    }

    Ok(())
}

/// Fix serialized redacted state events.
async fn migrate_to_v3(db: Database, store_cipher: Option<&StoreCipher>) -> Result<Database> {
    let tx = db
        .transaction([keys::ROOM_STATE, keys::ROOM_INFOS])
        .with_mode(TransactionMode::Readwrite)
        .build()?;

    v3_fix_store(&tx.object_store(keys::ROOM_STATE)?, store_cipher).await?;
    v3_fix_store(&tx.object_store(keys::ROOM_INFOS)?, store_cipher).await?;

    tx.commit().await?;

    let name = db.name();
    db.close();

    // Update the version of the database.
    Ok(Database::open(&name).with_version(3u32).await?)
}

/// Move the content of the SYNC_TOKEN and SESSION stores to the new KV store.
async fn migrate_to_v4(db: Database, store_cipher: Option<&StoreCipher>) -> Result<Database> {
    let tx = db
        .transaction([old_keys::SYNC_TOKEN, old_keys::SESSION])
        .with_mode(TransactionMode::Readonly)
        .build()?;
    let mut values = Vec::new();

    // Sync token
    let sync_token_store = tx.object_store(old_keys::SYNC_TOKEN)?;
    let sync_token = sync_token_store.get(&JsValue::from_str(old_keys::SYNC_TOKEN)).await?;

    if let Some(sync_token) = sync_token {
        values.push((
            encode_key(store_cipher, StateStoreDataKey::SYNC_TOKEN, StateStoreDataKey::SYNC_TOKEN),
            sync_token,
        ));
    }

    // Filters
    let session_store = tx.object_store(old_keys::SESSION)?;
    let range = encode_to_range(store_cipher, StateStoreDataKey::FILTER, StateStoreDataKey::FILTER);
    if let Some(mut cursor) = session_store.open_cursor().with_query(&range).await? {
        while let Some(value) = cursor.next_record().await? {
            let Some(key) = cursor.key()? else {
                break;
            };
            values.push((key, value));
        }
    }

    tx.commit().await?;

    let mut data = HashMap::new();
    if !values.is_empty() {
        data.insert(keys::KV, values);
    }

    let migration = OngoingMigration {
        drop_stores: [old_keys::SYNC_TOKEN, old_keys::SESSION].into_iter().collect(),
        create_stores: [keys::KV].into_iter().collect(),
        data,
    };
    apply_migration(db, 4, migration).await
}

/// Move the member events with other state events.
async fn migrate_to_v5(db: Database, store_cipher: Option<&StoreCipher>) -> Result<Database> {
    let tx = db
        .transaction([
            old_keys::MEMBERS,
            old_keys::STRIPPED_MEMBERS,
            keys::ROOM_STATE,
            keys::STRIPPED_ROOM_STATE,
            keys::ROOM_INFOS,
            old_keys::STRIPPED_ROOM_INFOS,
        ])
        .with_mode(TransactionMode::Readwrite)
        .build()?;

    let members_store = tx.object_store(old_keys::MEMBERS)?;
    let state_store = tx.object_store(keys::ROOM_STATE)?;
    let room_infos = tx
        .object_store(keys::ROOM_INFOS)?
        .get_all()
        .await?
        .filter_map(Result::ok)
        .filter_map(|f| deserialize_value::<RoomInfoV1>(store_cipher, &f).ok())
        .collect::<Vec<_>>();

    for room_info in room_infos {
        let room_id = room_info.room_id();
        let range = encode_to_range(store_cipher, old_keys::MEMBERS, room_id);
        for result in members_store.get_all().with_query(&range).await? {
            let value = result?;
            let raw_member_event =
                deserialize_value::<Raw<SyncRoomMemberEvent>>(store_cipher, &value)?;
            let state_key = raw_member_event.get_field::<String>("state_key")?.unwrap_or_default();
            let key = encode_key(
                store_cipher,
                keys::ROOM_STATE,
                (room_id, StateEventType::RoomMember, state_key),
            );

            state_store.add(&value).with_key(key).build()?.await?;
        }
    }

    let stripped_members_store = tx.object_store(old_keys::STRIPPED_MEMBERS)?;
    let stripped_state_store = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
    let stripped_room_infos = tx
        .object_store(old_keys::STRIPPED_ROOM_INFOS)?
        .get_all()
        .await?
        .filter_map(Result::ok)
        .filter_map(|f| deserialize_value::<RoomInfoV1>(store_cipher, &f).ok())
        .collect::<Vec<_>>();

    for room_info in stripped_room_infos {
        let room_id = room_info.room_id();
        let range = encode_to_range(store_cipher, old_keys::STRIPPED_MEMBERS, room_id);
        for result in stripped_members_store.get_all().with_query(&range).await? {
            let value = result?;
            let raw_member_event =
                deserialize_value::<Raw<StrippedRoomMemberEvent>>(store_cipher, &value)?;
            let state_key = raw_member_event.get_field::<String>("state_key")?.unwrap_or_default();
            let key = encode_key(
                store_cipher,
                keys::STRIPPED_ROOM_STATE,
                (room_id, StateEventType::RoomMember, state_key),
            );

            stripped_state_store.add(&value).with_key(key).build()?.await?;
        }
    }

    tx.commit().await?;

    let migration = OngoingMigration {
        drop_stores: [old_keys::MEMBERS, old_keys::STRIPPED_MEMBERS].into_iter().collect(),
        create_stores: Default::default(),
        data: Default::default(),
    };
    apply_migration(db, 5, migration).await
}

/// Remove the old user IDs stores and populate the new ones.
async fn migrate_to_v6(db: Database, store_cipher: Option<&StoreCipher>) -> Result<Database> {
    // We only have joined and invited user IDs in the old store, so instead we will
    // use the room member events to populate the new store.
    let tx = db
        .transaction([
            keys::ROOM_STATE,
            keys::ROOM_INFOS,
            keys::STRIPPED_ROOM_STATE,
            old_keys::STRIPPED_ROOM_INFOS,
        ])
        .with_mode(TransactionMode::Readonly)
        .build()?;

    let state_store = tx.object_store(keys::ROOM_STATE)?;
    let room_infos = tx
        .object_store(keys::ROOM_INFOS)?
        .get_all()
        .await?
        .filter_map(Result::ok)
        .filter_map(|f| deserialize_value::<RoomInfoV1>(store_cipher, &f).ok())
        .collect::<Vec<_>>();
    let mut values = Vec::new();

    for room_info in room_infos {
        let room_id = room_info.room_id();
        let range =
            encode_to_range(store_cipher, keys::ROOM_STATE, (room_id, StateEventType::RoomMember));
        for result in state_store.get_all().with_query(&range).await? {
            let value = result?;
            let member_event = deserialize_value::<Raw<SyncRoomMemberEvent>>(store_cipher, &value)?
                .deserialize()?;
            let key = encode_key(store_cipher, keys::USER_IDS, (room_id, member_event.state_key()));
            let value = serialize_value(store_cipher, &RoomMember::from(&member_event))?;

            values.push((key, value));
        }
    }

    let stripped_state_store = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
    let stripped_room_infos = tx
        .object_store(old_keys::STRIPPED_ROOM_INFOS)?
        .get_all()
        .await?
        .filter_map(Result::ok)
        .filter_map(|f| deserialize_value::<RoomInfoV1>(store_cipher, &f).ok())
        .collect::<Vec<_>>();
    let mut stripped_values = Vec::new();

    for room_info in stripped_room_infos {
        let room_id = room_info.room_id();
        let range = encode_to_range(
            store_cipher,
            keys::STRIPPED_ROOM_STATE,
            (room_id, StateEventType::RoomMember),
        );
        for result in stripped_state_store.get_all().with_query(&range).await? {
            let value = result?;
            let stripped_member_event =
                deserialize_value::<Raw<StrippedRoomMemberEvent>>(store_cipher, &value)?
                    .deserialize()?;
            let key = encode_key(
                store_cipher,
                keys::STRIPPED_USER_IDS,
                (room_id, &stripped_member_event.state_key),
            );
            let value = serialize_value(store_cipher, &RoomMember::from(&stripped_member_event))?;

            stripped_values.push((key, value));
        }
    }

    tx.commit().await?;

    let mut data = HashMap::new();
    if !values.is_empty() {
        data.insert(keys::USER_IDS, values);
    }
    if !stripped_values.is_empty() {
        data.insert(keys::STRIPPED_USER_IDS, stripped_values);
    }

    let migration = OngoingMigration {
        drop_stores: HashSet::from_iter([
            old_keys::JOINED_USER_IDS,
            old_keys::INVITED_USER_IDS,
            old_keys::STRIPPED_JOINED_USER_IDS,
            old_keys::STRIPPED_INVITED_USER_IDS,
        ]),
        create_stores: HashSet::from_iter([keys::USER_IDS, keys::STRIPPED_USER_IDS]),
        data,
    };
    apply_migration(db, 6, migration).await
}

/// Remove the stripped room infos store and migrate the data with the other
/// room infos, as well as .
async fn migrate_to_v7(db: Database, store_cipher: Option<&StoreCipher>) -> Result<Database> {
    let tx = db
        .transaction([old_keys::STRIPPED_ROOM_INFOS])
        .with_mode(TransactionMode::Readonly)
        .build()?;

    let room_infos = tx
        .object_store(old_keys::STRIPPED_ROOM_INFOS)?
        .get_all()
        .await?
        .filter_map(Result::ok)
        .filter_map(|value| {
            deserialize_value::<RoomInfoV1>(store_cipher, &value)
                .ok()
                .map(|info| (encode_key(store_cipher, keys::ROOM_INFOS, info.room_id()), value))
        })
        .collect::<Vec<_>>();

    tx.commit().await?;

    let mut data = HashMap::new();
    if !room_infos.is_empty() {
        data.insert(keys::ROOM_INFOS, room_infos);
    }

    let migration = OngoingMigration {
        drop_stores: HashSet::from_iter([old_keys::STRIPPED_ROOM_INFOS]),
        data,
        ..Default::default()
    };
    apply_migration(db, 7, migration).await
}

/// Change the format of the room infos.
async fn migrate_to_v8(db: Database, store_cipher: Option<&StoreCipher>) -> Result<Database> {
    let tx = db
        .transaction([keys::ROOM_STATE, keys::STRIPPED_ROOM_STATE, keys::ROOM_INFOS])
        .with_mode(TransactionMode::Readwrite)
        .build()?;

    let room_state_store = tx.object_store(keys::ROOM_STATE)?;
    let stripped_room_state_store = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
    let room_infos_store = tx.object_store(keys::ROOM_INFOS)?;

    let room_infos_v1 = room_infos_store
        .get_all()
        .build()?
        .await?
        .map(|value| deserialize_value::<RoomInfoV1>(store_cipher, &value?))
        .collect::<Result<Vec<_>, _>>()?;

    for room_info_v1 in room_infos_v1 {
        let create = if let Some(event) = stripped_room_state_store
            .get(&encode_key(
                store_cipher,
                keys::STRIPPED_ROOM_STATE,
                (room_info_v1.room_id(), &StateEventType::RoomCreate, ""),
            ))
            .await?
            .map(|f| deserialize_value(store_cipher, &f))
            .transpose()?
        {
            Some(SyncOrStrippedState::<RoomCreateEventContent>::Stripped(event))
        } else {
            room_state_store
                .get(&encode_key(
                    store_cipher,
                    keys::ROOM_STATE,
                    (room_info_v1.room_id(), &StateEventType::RoomCreate, ""),
                ))
                .await?
                .map(|f| deserialize_value(store_cipher, &f))
                .transpose()?
                .map(SyncOrStrippedState::<RoomCreateEventContent>::Sync)
        };

        let room_info = room_info_v1.migrate(create.as_ref());
        room_infos_store
            .put(&serialize_value(store_cipher, &room_info)?)
            .with_key(encode_key(store_cipher, keys::ROOM_INFOS, room_info.room_id()))
            .build()?;
    }

    tx.commit().await?;

    let name = db.name();
    db.close();

    // Update the version of the database.
    Ok(Database::open(&name).with_version(8u32).build()?.await?)
}

/// Add the new [`keys::ROOM_SEND_QUEUE`] table.
async fn migrate_to_v9(db: Database) -> Result<Database> {
    let migration = OngoingMigration {
        drop_stores: [].into(),
        create_stores: [keys::ROOM_SEND_QUEUE].into_iter().collect(),
        data: Default::default(),
    };
    apply_migration(db, 9, migration).await
}

/// Add the new [`keys::DEPENDENT_SEND_QUEUE`] table.
async fn migrate_to_v10(db: Database) -> Result<Database> {
    let migration = OngoingMigration {
        drop_stores: [].into(),
        create_stores: [keys::DEPENDENT_SEND_QUEUE].into_iter().collect(),
        data: Default::default(),
    };
    apply_migration(db, 10, migration).await
}

/// Drop the [`old_keys::MEDIA`] table.
async fn migrate_to_v11(db: Database) -> Result<Database> {
    let migration = OngoingMigration {
        drop_stores: [old_keys::MEDIA].into(),
        create_stores: Default::default(),
        data: Default::default(),
    };
    apply_migration(db, 11, migration).await
}

/// The format of data serialized into the send queue and dependent send queue
/// tables have changed, clear both.
async fn migrate_to_v12(db: Database) -> Result<Database> {
    let store_keys = [keys::DEPENDENT_SEND_QUEUE, keys::ROOM_SEND_QUEUE];
    let tx = db.transaction(store_keys).with_mode(TransactionMode::Readwrite).build()?;

    for store_name in store_keys {
        let store = tx.object_store(store_name)?;
        store.clear()?;
    }

    tx.commit().await?;

    let name = db.name();
    db.close();

    // Update the version of the database.
    Ok(Database::open(&name).with_version(12u32).build()?.await?)
}

/// Add the thread subscriptions table.
async fn migrate_to_v13(db: Database) -> Result<Database> {
    let migration = OngoingMigration {
        drop_stores: [].into(),
        create_stores: [keys::THREAD_SUBSCRIPTIONS].into_iter().collect(),
        data: Default::default(),
    };
    apply_migration(db, 13, migration).await
}

/// Empty the thread subscriptions table, because the serialized format has
/// changed (from storing only the subscription to storing the
/// `StoredThreadSubscription`).
async fn migrate_to_v14(db: Database) -> Result<Database> {
    let migration = OngoingMigration {
        drop_stores: [keys::THREAD_SUBSCRIPTIONS].into_iter().collect(),
        create_stores: [keys::THREAD_SUBSCRIPTIONS].into_iter().collect(),
        data: Default::default(),
    };
    apply_migration(db, 14, migration).await
}

#[cfg(all(test, target_family = "wasm"))]
mod tests {
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use indexed_db_futures::{
        database::{Database, VersionChangeEvent},
        error::Error,
        object_store::ObjectStore,
        prelude::*,
        transaction::{Transaction, TransactionMode},
    };
    use matrix_sdk_base::{
        RoomMemberships, RoomState, StateStore, StateStoreDataKey, StoreError,
        deserialized_responses::RawMemberEvent,
        store::{RoomLoadSettings, StateStoreExt},
        sync::UnreadNotificationsCount,
    };
    use matrix_sdk_test::{async_test, test_json};
    use ruma::{
        EventId, MilliSecondsSinceUnixEpoch, OwnedUserId, RoomId, UserId,
        events::{
            AnySyncStateEvent, StateEventType,
            room::{
                create::RoomCreateEventContent,
                member::{StrippedRoomMemberEvent, SyncRoomMemberEvent},
            },
        },
        owned_user_id, room_id,
        serde::Raw,
        server_name, user_id,
    };
    use serde_json::json;
    use uuid::Uuid;
    use wasm_bindgen::JsValue;

    use super::{CURRENT_DB_VERSION, CURRENT_META_DB_VERSION, MigrationConflictStrategy, old_keys};
    use crate::{
        IndexeddbStateStore, IndexeddbStateStoreError,
        serializer::safe_encode::traits::SafeEncode,
        state_store::{Result, encode_key, keys, serialize_value},
    };

    const CUSTOM_DATA_KEY: &[u8] = b"custom_data_key";
    const CUSTOM_DATA: &[u8] = b"some_custom_data";

    pub async fn create_fake_db(name: &str, version: u32) -> Result<Database> {
        Database::open(name)
            .with_version(version)
            .with_on_upgrade_needed(
                move |_: VersionChangeEvent, tx: &Transaction<'_>| -> Result<(), Error> {
                    let db = tx.db();

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
                        keys::CUSTOM,
                    ];

                    for name in common_stores {
                        db.create_object_store(name).build()?;
                    }

                    if version < 4 {
                        for name in [old_keys::SYNC_TOKEN, old_keys::SESSION] {
                            db.create_object_store(name).build()?;
                        }
                    }
                    if version >= 4 {
                        db.create_object_store(keys::KV).build()?;
                    }
                    if version < 5 {
                        for name in [old_keys::MEMBERS, old_keys::STRIPPED_MEMBERS] {
                            db.create_object_store(name).build()?;
                        }
                    }
                    if version < 6 {
                        for name in [
                            old_keys::INVITED_USER_IDS,
                            old_keys::JOINED_USER_IDS,
                            old_keys::STRIPPED_INVITED_USER_IDS,
                            old_keys::STRIPPED_JOINED_USER_IDS,
                        ] {
                            db.create_object_store(name).build()?;
                        }
                    }
                    if version >= 6 {
                        for name in [keys::USER_IDS, keys::STRIPPED_USER_IDS] {
                            db.create_object_store(name).build()?;
                        }
                    }
                    if version < 7 {
                        db.create_object_store(old_keys::STRIPPED_ROOM_INFOS).build()?;
                    }
                    if version < 11 {
                        db.create_object_store(old_keys::MEDIA).build()?;
                    }

                    Ok(())
                },
            )
            .build()?
            .await
            .map_err(Into::into)
    }

    fn room_info_v1_json(
        room_id: &RoomId,
        state: RoomState,
        name: Option<&str>,
        creator: Option<&UserId>,
    ) -> serde_json::Value {
        // Test with name set or not.
        let name_content = match name {
            Some(name) => json!({ "name": name }),
            None => json!({ "name": null }),
        };
        // Test with creator set or not.
        let create_content = match creator {
            Some(creator) => RoomCreateEventContent::new_v1(creator.to_owned()),
            None => RoomCreateEventContent::new_v11(),
        };

        json!({
            "room_id": room_id,
            "room_type": state,
            "notification_counts": UnreadNotificationsCount::default(),
            "summary": {
                "heroes": [],
                "joined_member_count": 0,
                "invited_member_count": 0,
            },
            "members_synced": false,
            "base_info": {
                "dm_targets": [],
                "max_power_level": 100,
                "name": {
                    "Original": {
                        "content": name_content,
                    },
                },
                "create": {
                    "Original": {
                        "content": create_content,
                    }
                }
            },
        })
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
            let tx = db.transaction(keys::CUSTOM).with_mode(TransactionMode::Readwrite).build()?;
            let custom = tx.object_store(keys::CUSTOM)?;
            let jskey = JsValue::from_str(
                core::str::from_utf8(CUSTOM_DATA_KEY).map_err(StoreError::Codec)?,
            );
            custom.put(&serialize_value(None, &CUSTOM_DATA)?).with_key(jskey).await?;
            tx.commit().await?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;
        // this didn't create any backup
        assert_eq!(store.has_backups().await?, false);
        // Custom data is still there.
        assert_let!(Some(stored_data) = store.get_custom_value(CUSTOM_DATA_KEY).await?);
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
            let tx = db.transaction(keys::CUSTOM).with_mode(TransactionMode::Readwrite).build()?;
            let custom = tx.object_store(keys::CUSTOM)?;
            let jskey = JsValue::from_str(
                core::str::from_utf8(CUSTOM_DATA_KEY).map_err(StoreError::Codec)?,
            );
            custom.put(&serialize_value(None, &CUSTOM_DATA)?).with_key(jskey).await?;
            tx.commit().await?;
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
            let tx = db.transaction(keys::CUSTOM).with_mode(TransactionMode::Readwrite).build()?;
            let custom = tx.object_store(keys::CUSTOM)?;
            let jskey = JsValue::from_str(
                core::str::from_utf8(CUSTOM_DATA_KEY).map_err(StoreError::Codec)?,
            );
            custom.put(&serialize_value(None, &CUSTOM_DATA)?).with_key(jskey).await?;
            tx.commit().await?;
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
            let tx = db.transaction(keys::CUSTOM).with_mode(TransactionMode::Readwrite).build()?;
            let custom = tx.object_store(keys::CUSTOM)?;
            let jskey = JsValue::from_str(
                core::str::from_utf8(CUSTOM_DATA_KEY).map_err(StoreError::Codec)?,
            );
            custom.put(&serialize_value(None, &CUSTOM_DATA)?).with_key(jskey).await?;
            tx.commit().await?;
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
                db.transaction(keys::ROOM_STATE).with_mode(TransactionMode::Readwrite).build()?;
            let state = tx.object_store(keys::ROOM_STATE)?;
            let key: JsValue = (room_id, StateEventType::RoomTopic, "").as_encoded_string().into();
            state.put(&serialize_value(None, &wrong_redacted_state_event)?).with_key(key).await?;
            tx.commit().await?;
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
            let tx = db
                .transaction([old_keys::SYNC_TOKEN, old_keys::SESSION])
                .with_mode(TransactionMode::Readwrite)
                .build()?;

            let sync_token_store = tx.object_store(old_keys::SYNC_TOKEN)?;
            sync_token_store
                .put(&serialize_value(None, &sync_token)?)
                .with_key(JsValue::from_str(old_keys::SYNC_TOKEN))
                .build()?;

            let session_store = tx.object_store(old_keys::SESSION)?;
            session_store
                .put(&serialize_value(None, &filter_1_id)?)
                .with_key(encode_key(
                    None,
                    StateStoreDataKey::FILTER,
                    (StateStoreDataKey::FILTER, filter_1),
                ))
                .build()?;
            session_store
                .put(&serialize_value(None, &filter_2_id)?)
                .with_key(encode_key(
                    None,
                    StateStoreDataKey::FILTER,
                    (StateStoreDataKey::FILTER, filter_2),
                ))
                .build()?;

            tx.commit().await?;
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
            Raw::new(&*test_json::MEMBER_INVITE).unwrap().cast_unchecked::<SyncRoomMemberEvent>();
        let user_id = user_id!("@invited:localhost");

        let stripped_room_id = room_id!("!stripped_room:localhost");
        let stripped_member_event = Raw::new(&*test_json::MEMBER_STRIPPED)
            .unwrap()
            .cast_unchecked::<StrippedRoomMemberEvent>();
        let stripped_user_id = user_id!("@example:localhost");

        // Populate DB with old table.
        {
            let db = create_fake_db(&name, 4).await?;
            let tx = db
                .transaction([
                    old_keys::MEMBERS,
                    keys::ROOM_INFOS,
                    old_keys::STRIPPED_MEMBERS,
                    old_keys::STRIPPED_ROOM_INFOS,
                ])
                .with_mode(TransactionMode::Readwrite)
                .build()?;

            let members_store = tx.object_store(old_keys::MEMBERS)?;
            members_store
                .put(&serialize_value(None, &member_event)?)
                .with_key(encode_key(None, old_keys::MEMBERS, (room_id, user_id)))
                .build()?;
            let room_infos_store = tx.object_store(keys::ROOM_INFOS)?;
            let room_info = room_info_v1_json(room_id, RoomState::Joined, None, None);
            room_infos_store
                .put(&serialize_value(None, &room_info)?)
                .with_key(encode_key(None, keys::ROOM_INFOS, room_id))
                .build()?;

            let stripped_members_store = tx.object_store(old_keys::STRIPPED_MEMBERS)?;
            stripped_members_store
                .put(&serialize_value(None, &stripped_member_event)?)
                .with_key(encode_key(
                    None,
                    old_keys::STRIPPED_MEMBERS,
                    (stripped_room_id, stripped_user_id),
                ))
                .build()?;
            let stripped_room_infos_store = tx.object_store(old_keys::STRIPPED_ROOM_INFOS)?;
            let stripped_room_info =
                room_info_v1_json(stripped_room_id, RoomState::Invited, None, None);
            stripped_room_infos_store
                .put(&serialize_value(None, &stripped_room_info)?)
                .with_key(encode_key(None, old_keys::STRIPPED_ROOM_INFOS, stripped_room_id))
                .build()?;

            tx.commit().await?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;

        assert_let!(
            Ok(Some(RawMemberEvent::Sync(stored_member_event))) =
                store.get_member_event(room_id, user_id).await
        );
        assert_eq!(stored_member_event.json().get(), member_event.json().get());

        assert_let!(
            Ok(Some(RawMemberEvent::Stripped(stored_stripped_member_event))) =
                store.get_member_event(stripped_room_id, stripped_user_id).await
        );
        assert_eq!(stored_stripped_member_event.json().get(), stripped_member_event.json().get());

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_to_v6() -> Result<()> {
        let name = format!("migrating-v6-{}", Uuid::new_v4().as_hyphenated().to_string());

        let room_id = room_id!("!room:localhost");
        let invite_member_event =
            Raw::new(&*test_json::MEMBER_INVITE).unwrap().cast_unchecked::<SyncRoomMemberEvent>();
        let invite_user_id = user_id!("@invited:localhost");
        let ban_member_event =
            Raw::new(&*test_json::MEMBER_BAN).unwrap().cast_unchecked::<SyncRoomMemberEvent>();
        let ban_user_id = user_id!("@banned:localhost");

        let stripped_room_id = room_id!("!stripped_room:localhost");
        let stripped_member_event = Raw::new(&*test_json::MEMBER_STRIPPED)
            .unwrap()
            .cast_unchecked::<StrippedRoomMemberEvent>();
        let stripped_user_id = user_id!("@example:localhost");

        // Populate DB with old table.
        {
            let db = create_fake_db(&name, 5).await?;
            let tx = db
                .transaction([
                    keys::ROOM_STATE,
                    keys::ROOM_INFOS,
                    keys::STRIPPED_ROOM_STATE,
                    old_keys::STRIPPED_ROOM_INFOS,
                    old_keys::INVITED_USER_IDS,
                    old_keys::JOINED_USER_IDS,
                    old_keys::STRIPPED_INVITED_USER_IDS,
                    old_keys::STRIPPED_JOINED_USER_IDS,
                ])
                .with_mode(TransactionMode::Readwrite)
                .build()?;

            let state_store = tx.object_store(keys::ROOM_STATE)?;
            state_store
                .put(&serialize_value(None, &invite_member_event)?)
                .with_key(encode_key(
                    None,
                    keys::ROOM_STATE,
                    (room_id, StateEventType::RoomMember, invite_user_id),
                ))
                .build()?;
            state_store
                .put(&serialize_value(None, &ban_member_event)?)
                .with_key(encode_key(
                    None,
                    keys::ROOM_STATE,
                    (room_id, StateEventType::RoomMember, ban_user_id),
                ))
                .build()?;
            let room_infos_store = tx.object_store(keys::ROOM_INFOS)?;
            let room_info = room_info_v1_json(room_id, RoomState::Joined, None, None);
            room_infos_store
                .put(&serialize_value(None, &room_info)?)
                .with_key(encode_key(None, keys::ROOM_INFOS, room_id))
                .build()?;

            let stripped_state_store = tx.object_store(keys::STRIPPED_ROOM_STATE)?;
            stripped_state_store
                .put(&serialize_value(None, &stripped_member_event)?)
                .with_key(encode_key(
                    None,
                    keys::STRIPPED_ROOM_STATE,
                    (stripped_room_id, StateEventType::RoomMember, stripped_user_id),
                ))
                .build()?;
            let stripped_room_infos_store = tx.object_store(old_keys::STRIPPED_ROOM_INFOS)?;
            let stripped_room_info =
                room_info_v1_json(stripped_room_id, RoomState::Invited, None, None);
            stripped_room_infos_store
                .put(&serialize_value(None, &stripped_room_info)?)
                .with_key(encode_key(None, old_keys::STRIPPED_ROOM_INFOS, stripped_room_id))
                .build()?;

            // Populate the old user IDs stores to check the data is not reused.
            let joined_user_id = user_id!("@joined_user:localhost");
            tx.object_store(old_keys::JOINED_USER_IDS)?
                .put(&serialize_value(None, &joined_user_id)?)
                .with_key(encode_key(None, old_keys::JOINED_USER_IDS, (room_id, joined_user_id)))
                .build()?;
            let invited_user_id = user_id!("@invited_user:localhost");
            tx.object_store(old_keys::INVITED_USER_IDS)?
                .put(&serialize_value(None, &invited_user_id)?)
                .with_key(encode_key(None, old_keys::INVITED_USER_IDS, (room_id, invited_user_id)))
                .build()?;
            let stripped_joined_user_id = user_id!("@stripped_joined_user:localhost");
            tx.object_store(old_keys::STRIPPED_JOINED_USER_IDS)?
                .put(&serialize_value(None, &stripped_joined_user_id)?)
                .with_key(encode_key(
                    None,
                    old_keys::STRIPPED_JOINED_USER_IDS,
                    (room_id, stripped_joined_user_id),
                ))
                .build()?;
            let stripped_invited_user_id = user_id!("@stripped_invited_user:localhost");
            tx.object_store(old_keys::STRIPPED_INVITED_USER_IDS)?
                .put(&serialize_value(None, &stripped_invited_user_id)?)
                .with_key(encode_key(
                    None,
                    old_keys::STRIPPED_INVITED_USER_IDS,
                    (room_id, stripped_invited_user_id),
                ))
                .build()?;

            tx.commit().await?;
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
            let tx = db
                .transaction([keys::ROOM_INFOS, old_keys::STRIPPED_ROOM_INFOS])
                .with_mode(TransactionMode::Readwrite)
                .build()?;

            let room_infos_store = tx.object_store(keys::ROOM_INFOS)?;
            let room_info = room_info_v1_json(room_id, RoomState::Joined, None, None);
            room_infos_store
                .put(&serialize_value(None, &room_info)?)
                .with_key(encode_key(None, keys::ROOM_INFOS, room_id))
                .build()?;

            let stripped_room_infos_store = tx.object_store(old_keys::STRIPPED_ROOM_INFOS)?;
            let stripped_room_info =
                room_info_v1_json(stripped_room_id, RoomState::Invited, None, None);
            stripped_room_infos_store
                .put(&serialize_value(None, &stripped_room_info)?)
                .with_key(encode_key(None, old_keys::STRIPPED_ROOM_INFOS, stripped_room_id))
                .build()?;

            tx.commit().await?;
            db.close();
        }

        // this transparently migrates to the latest version
        let store = IndexeddbStateStore::builder().name(name).build().await?;

        assert_eq!(store.get_room_infos(&RoomLoadSettings::default()).await.unwrap().len(), 2);

        Ok(())
    }

    // Add a room in version 7 format of the state store.
    fn add_room_v7(
        room_infos_store: &ObjectStore<'_>,
        room_state_store: &ObjectStore<'_>,
        room_id: &RoomId,
        name: Option<&str>,
        create_creator: Option<OwnedUserId>,
        create_sender: Option<&UserId>,
    ) -> Result<()> {
        let room_info_json =
            room_info_v1_json(room_id, RoomState::Joined, name, create_creator.as_deref());

        room_infos_store
            .put(&serialize_value(None, &room_info_json)?)
            .with_key(encode_key(None, keys::ROOM_INFOS, room_id))
            .build()?;

        // Test with or without `m.room.create` event in the room state.
        let Some(create_sender) = create_sender else {
            return Ok(());
        };

        let create_content = match create_creator {
            Some(creator) => RoomCreateEventContent::new_v1(creator),
            None => RoomCreateEventContent::new_v11(),
        };

        let event_id = EventId::new(server_name!("dummy.local"));
        let create_event = json!({
            "content": create_content,
            "event_id": event_id,
            "sender": create_sender,
            "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
            "state_key": "",
            "type": "m.room.create",
            "unsigned": {},
        });

        room_state_store
            .put(&serialize_value(None, &create_event)?)
            .with_key(encode_key(
                None,
                keys::ROOM_STATE,
                (room_id, &StateEventType::RoomCreate, ""),
            ))
            .build()?;

        Ok(())
    }

    #[async_test]
    pub async fn test_migrating_to_v8() -> Result<()> {
        let name = format!("migrating-v8-{}", Uuid::new_v4().as_hyphenated().to_string());

        // Room A: with name, creator and sender.
        let room_a_id = room_id!("!room_a:dummy.local");
        let room_a_name = "Room A";
        let room_a_creator = owned_user_id!("@creator:dummy.local");
        // Use a different sender to check that sender is used over creator in
        // migration.
        let room_a_create_sender = owned_user_id!("@sender:dummy.local");

        // Room B: without name, creator and sender.
        let room_b_id = room_id!("!room_b:dummy.local");

        // Room C: only with sender.
        let room_c_id = room_id!("!room_c:dummy.local");
        let room_c_create_sender = owned_user_id!("@creator:dummy.local");

        // Create and populate db.
        {
            let db = create_fake_db(&name, 6).await?;
            let tx = db
                .transaction([keys::ROOM_INFOS, keys::ROOM_STATE])
                .with_mode(TransactionMode::Readwrite)
                .build()?;

            let room_infos_store = tx.object_store(keys::ROOM_INFOS)?;
            let room_state_store = tx.object_store(keys::ROOM_STATE)?;

            add_room_v7(
                &room_infos_store,
                &room_state_store,
                room_a_id,
                Some(room_a_name),
                Some(room_a_creator),
                Some(&room_a_create_sender),
            )?;
            add_room_v7(&room_infos_store, &room_state_store, room_b_id, None, None, None)?;
            add_room_v7(
                &room_infos_store,
                &room_state_store,
                room_c_id,
                None,
                None,
                Some(&room_c_create_sender),
            )?;

            tx.commit().await?;
            db.close();
        }

        // This transparently migrates to the latest version.
        let store = IndexeddbStateStore::builder().name(name).build().await?;

        // Check all room infos are there.
        let room_infos = store.get_room_infos(&RoomLoadSettings::default()).await?;
        assert_eq!(room_infos.len(), 3);

        let room_a = room_infos.iter().find(|r| r.room_id() == room_a_id).unwrap();
        assert_eq!(room_a.name(), Some(room_a_name));
        assert_eq!(room_a.creators(), Some(vec![room_a_create_sender]));

        let room_b = room_infos.iter().find(|r| r.room_id() == room_b_id).unwrap();
        assert_eq!(room_b.name(), None);
        assert_eq!(room_b.creators(), None);

        let room_c = room_infos.iter().find(|r| r.room_id() == room_c_id).unwrap();
        assert_eq!(room_c.name(), None);
        assert_eq!(room_c.creators(), Some(vec![room_c_create_sender]));

        Ok(())
    }
}
