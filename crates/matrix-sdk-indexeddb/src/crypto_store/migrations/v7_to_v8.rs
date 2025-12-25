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

//! Migration code that modifies the data inside inbound_group_sessions2,
//! ensuring that the keys are correctly encoded for this new store name.

use indexed_db_futures::{
    Build, error::OpenDbError, query_source::QuerySource, transaction::TransactionMode,
};
use matrix_sdk_crypto::olm::InboundGroupSession;
use tracing::{debug, info};
use wasm_bindgen::JsValue;

use crate::{
    IndexeddbCryptoStoreError,
    crypto_store::{
        Result,
        migrations::{MigrationDb, do_schema_upgrade, old_keys, v7},
    },
    serializer::SafeEncodeSerializer,
};

/// In the migration v5 to v7, we incorrectly copied the keys in
/// `inbound_group_sessions` verbatim into `inbound_group_sessions2`. What we
/// should have done is re-hash them using the new table name, so we fix them up
/// here.
pub(crate) async fn data_migrate(name: &str, serializer: &SafeEncodeSerializer) -> Result<()> {
    let db = MigrationDb::new(name, 8).await?;

    let txn = db
        .transaction(old_keys::INBOUND_GROUP_SESSIONS_V2)
        .with_mode(TransactionMode::Readwrite)
        .build()?;

    let store = txn.object_store(old_keys::INBOUND_GROUP_SESSIONS_V2)?;

    let row_count = store.count().await?;
    info!(row_count, "Fixing inbound group session data keys");

    // Iterate through all rows
    if let Some(mut cursor) = store.open_cursor().await? {
        let mut idx = 0;
        let mut updated = 0;
        let mut deleted = 0;
        while let Some(value) = cursor.next_record::<JsValue>().await? {
            idx += 1;

            // Get the old key and session

            let old_key =
                cursor.key::<JsValue>()?.ok_or(matrix_sdk_crypto::CryptoStoreError::Backend(
                    "inbound_group_sessions2 cursor has no key".into(),
                ))?;

            let idb_object: v7::InboundGroupSessionIndexedDbObject2 =
                serde_wasm_bindgen::from_value(value)?;
            let pickled_session =
                serializer.deserialize_value_from_bytes(&idb_object.pickled_session)?;
            let session = InboundGroupSession::from_pickle(pickled_session)
                .map_err(|e| IndexeddbCryptoStoreError::CryptoStoreError(e.into()))?;

            if idx % 100 == 0 {
                debug!("Migrating session {idx} of {row_count}");
            }

            // Work out what the key should be.
            // (This is much the same as in
            // `IndexeddbCryptoStore::get_inbound_group_session`)
            let new_key = serializer.encode_key(
                old_keys::INBOUND_GROUP_SESSIONS_V2,
                (&session.room_id, session.session_id()),
            );

            if new_key != old_key {
                // We have found an entry that is stored under the incorrect old key

                // Delete the old entry under the wrong key
                cursor.delete()?;

                // Check for an existing entry with the new key
                let new_value = store.get::<JsValue, _, _>(&new_key).await?;

                // If we found an existing entry, it is more up-to-date, so we don't need to do
                // anything more.

                // If we didn't find an existing entry, we must create one with the correct key
                if new_value.is_none() {
                    store
                        .add(&serde_wasm_bindgen::to_value(&idb_object)?)
                        .with_key(new_key)
                        .build()?;
                    updated += 1;
                } else {
                    deleted += 1;
                }
            }
        }

        debug!(
            "Migrated {row_count} sessions: {updated} keys updated \
                     and {deleted} obsolete entries deleted."
        );
    }

    txn.commit().await?;
    Ok(())
}

/// Perform the schema upgrade v7 to v8, Just bumping the schema version.
pub(crate) async fn schema_bump(name: &str) -> Result<(), OpenDbError> {
    do_schema_upgrade(name, 8, |_, _| {
        // Just bump the version number to 8 to demonstrate that we have run the data
        // changes from prepare_data_for_v8.
        Ok(())
    })
    .await
}
