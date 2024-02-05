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

//! Migration code that moves from inbound_group_sessions to
//! inbound_group_sessions2, adding a `needs_backup` property.
//!
//! The migration 5->6 creates the new store inbound_group_sessions2.
//! Then we move the data into the new store.
//! The migration 6->7 deletes the old store inbound_group_sessions.

use indexed_db_futures::{
    request::{IdbOpenDbRequestLike, OpenDbRequest},
    IdbDatabase, IdbKeyPath, IdbQuerySource, IdbVersionChangeEvent,
};
use matrix_sdk_crypto::olm::InboundGroupSession;
use tracing::{debug, info};
use wasm_bindgen::JsValue;
use web_sys::{DomException, IdbIndexParameters, IdbTransactionMode};

use crate::{
    crypto_store::{
        indexeddb_serializer::IndexeddbSerializer,
        keys,
        migrations::{do_schema_upgrade, old_keys, v8_to_v10},
        Result,
    },
    IndexeddbCryptoStoreError,
};

pub(crate) async fn migrate_schema_up_to_v6(name: &str) -> Result<(), DomException> {
    do_schema_upgrade(name, 6, |db| {
        migrate_stores_to_v6(db)?;
        Ok(())
    })
    .await
}

fn migrate_stores_to_v6(db: &IdbDatabase) -> Result<(), DomException> {
    // We want to change the shape of the inbound group sessions store. To do so, we
    // first need to build a new store, then copy all the data over.
    //
    // But copying the data needs to happen outside the database upgrade process
    // (because it needs async calls). So, here we create a new store for
    // inbound group sessions. We don't populate it yet; that happens once we
    // have done the upgrade to v6, in `prepare_data_for_v7`. Finally we drop the
    // old store in create_stores_for_v7.

    let object_store = db.create_object_store(old_keys::INBOUND_GROUP_SESSIONS_V2)?;

    let mut params = IdbIndexParameters::new();
    params.unique(false);
    object_store.create_index_with_params(
        keys::INBOUND_GROUP_SESSIONS_BACKUP_INDEX,
        &IdbKeyPath::str("needs_backup"),
        &params,
    )?;

    Ok(())
}

pub(crate) async fn prepare_data_for_v7(
    name: &str,
    serializer: &IndexeddbSerializer,
) -> Result<()> {
    info!("IndexeddbCryptoStore migrate data before v7 starting");
    let db = IdbDatabase::open(name)?.await?;
    let res = do_prepare_data_for_v7(serializer, &db).await;
    db.close();
    res?;
    info!("IndexeddbCryptoStore migrate data before v7 finished");
    Ok(())
}

async fn do_prepare_data_for_v7(serializer: &IndexeddbSerializer, db: &IdbDatabase) -> Result<()> {
    // The new store has been made for inbound group sessions; time to populate it.
    let txn = db.transaction_on_multi_with_mode(
        &[old_keys::INBOUND_GROUP_SESSIONS_V1, old_keys::INBOUND_GROUP_SESSIONS_V2],
        IdbTransactionMode::Readwrite,
    )?;

    let old_store = txn.object_store(old_keys::INBOUND_GROUP_SESSIONS_V1)?;
    let new_store = txn.object_store(old_keys::INBOUND_GROUP_SESSIONS_V2)?;

    let row_count = old_store.count()?.await?;
    info!(row_count, "Migrating inbound group session data from v1 to v2");

    if let Some(cursor) = old_store.open_cursor()?.await? {
        let mut idx = 0;
        loop {
            idx += 1;
            let key = cursor.key().ok_or(matrix_sdk_crypto::CryptoStoreError::Backend(
                "inbound_group_sessions v1 cursor has no key".into(),
            ))?;
            let value = cursor.value();

            if idx % 100 == 0 {
                debug!("Migrating session {idx} of {row_count}");
            }

            let igs = InboundGroupSession::from_pickle(serializer.deserialize_value(value)?)
                .map_err(|e| IndexeddbCryptoStoreError::CryptoStoreError(e.into()))?;

            let new_data =
                serde_wasm_bindgen::to_value(&v8_to_v10::InboundGroupSessionIndexedDbObject2 {
                    pickled_session: serializer.serialize_value_as_bytes(&igs.pickle().await)?,
                    needs_backup: !igs.backed_up(),
                })?;

            new_store.add_key_val(&key, &new_data)?;

            // we are done with the original data, so delete it now.
            cursor.delete()?;

            if !cursor.continue_cursor()?.await? {
                break;
            }
        }
    }

    // We have finished with the old store. Clear it, since it is faster to
    // clear+delete than just delete. See https://www.artificialworlds.net/blog/2024/02/01/deleting-an-indexed-db-store-can-be-incredibly-slow-on-firefox/
    // for more details.
    old_store.clear()?.await?;

    Ok(txn.await.into_result()?)
}

pub(crate) async fn migrate_schema_for_v7(name: &str) -> Result<(), DomException> {
    let mut db_req: OpenDbRequest = IdbDatabase::open_u32(name, 7)?;
    db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        let old_version = evt.old_version() as u32;
        let new_version = evt.old_version() as u32;

        if old_version < 7 {
            info!(old_version, new_version, "IndexeddbCryptoStore upgrade schema -> v7 starting");
            migrate_stores_to_v7(evt.db())?;
            info!(old_version, new_version, "IndexeddbCryptoStore upgrade schema -> v7 complete");
        }

        Ok(())
    }));
    db_req.await?.close();
    Ok(())
}

fn migrate_stores_to_v7(db: &IdbDatabase) -> Result<(), DomException> {
    db.delete_object_store(old_keys::INBOUND_GROUP_SESSIONS_V1)
}
