/*
Copyright 2025 The Matrix.org Foundation C.I.C.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

//! Perform the schema upgrade v14 to v101: we create a new table for the
//! "withheld" session data, then migrate the existing data into it, swapping
//! the key around; finally, we drop the old table.

use indexed_db_futures::{
    Build, error::OpenDbError, query_source::QuerySource, transaction::TransactionMode,
};
use matrix_sdk_crypto::store::types::RoomKeyWithheldEntry;
use tracing::{debug, info, warn};
use wasm_bindgen::JsValue;

use super::{MigrationDb, old_keys};
use crate::{
    crypto_store::{Result, keys, migrations::do_schema_upgrade},
    serializer::SafeEncodeSerializer,
};

/// Migrate to schema v100: add the new `withheld_sessions` table.
pub(crate) async fn schema_add(name: &str) -> Result<(), OpenDbError> {
    do_schema_upgrade(name, 100, |tx, _| {
        tx.db().create_object_store(keys::WITHHELD_SESSIONS).build()?;
        Ok(())
    })
    .await
}

/// Migrate data from `direct_withheld_info` into `withheld_sessions`.
pub(crate) async fn data_migrate(name: &str, serializer: &SafeEncodeSerializer) -> Result<()> {
    let db = MigrationDb::new(name, 10).await?;

    let txn = db
        .transaction([old_keys::DIRECT_WITHHELD_INFO, keys::WITHHELD_SESSIONS])
        .with_mode(TransactionMode::Readwrite)
        .build()?;

    let old_store = txn.object_store(old_keys::DIRECT_WITHHELD_INFO)?;
    let new_store = txn.object_store(keys::WITHHELD_SESSIONS)?;

    let row_count = old_store.count().await?;
    info!(row_count, "Migrating withheld_sessions data");

    // Iterate through all rows
    if let Some(mut cursor) = old_store.open_cursor().await? {
        let mut idx = 0;
        while let Some(value) = cursor.next_record::<JsValue>().await? {
            idx += 1;

            if idx % 100 == 0 {
                debug!("Migrating withheld session {idx} of {row_count}");
            }

            // Deserialize the session from the old store
            let deserialized: RoomKeyWithheldEntry = serializer.deserialize_value(value.clone())?;

            // Calculate its key in the new table
            if let (Some(room_id), Some(session_id)) =
                (deserialized.content.room_id(), deserialized.content.megolm_session_id())
            {
                let new_key = serializer.encode_key(keys::WITHHELD_SESSIONS, (room_id, session_id));

                // Write it to the new store
                new_store.add(&value).with_key(new_key).build()?;
            } else {
                warn!(
                    "Discarding withheld session with unknown room/session ID: {:?}",
                    deserialized
                );
            }

            // We are done with the original data, so delete it now.
            cursor.delete()?;
        }

        // Continue to the next record, or stop if we're done
        debug!("Migrated {idx} withheld sessions.");
    }

    txn.commit().await?;
    Ok(())
}

/// Migrate to schema v101: drop the old `direct_withheld_info` table.
pub(crate) async fn schema_delete(name: &str) -> Result<(), OpenDbError> {
    do_schema_upgrade(name, 101, |tx, _| {
        tx.db().delete_object_store(old_keys::DIRECT_WITHHELD_INFO)?;
        Ok(())
    })
    .await
}
