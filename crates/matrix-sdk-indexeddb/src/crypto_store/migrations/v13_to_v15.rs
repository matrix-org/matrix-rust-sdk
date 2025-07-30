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

use indexed_db_futures::{IdbKeyPath, IdbQuerySource};
use tracing::{debug, info};
use web_sys::{DomException, IdbTransactionMode};

use crate::{
    crypto_store::{
        deserialize_inbound_group_session, keys,
        migrations::{do_schema_upgrade, old_keys, v8_to_v10, MigrationDb},
        serialize_inbound_group_session, Result,
    },
    serializer::IndexeddbSerializer,
};

/// Perform the schema upgrade v13 to v14
///
/// This creates an identical object store to `inbound_group_sessions3`, but
/// adds index on `(curve_key, sender_data_type, session_id)`.
pub(crate) async fn schema_add(name: &str) -> Result<(), DomException> {
    do_schema_upgrade(name, 14, |db, _, _| {
        let object_store = db.create_object_store(keys::INBOUND_GROUP_SESSIONS_V4)?;
        v8_to_v10::index_add(&object_store)?;
        object_store.create_index(
            keys::INBOUND_GROUP_SESSIONS_SENDER_KEY_INDEX,
            &IdbKeyPath::str_sequence(&["sender_key", "sender_data_type", "session_id"]),
        )?;
        Ok(())
    })
    .await
}

/// Migrate data from `inbound_group_sessions3` into `inbound_group_sessions4`.
pub(crate) async fn data_migrate(name: &str, serializer: &IndexeddbSerializer) -> Result<()> {
    let db = MigrationDb::new(name, 15).await?;

    let txn = db.transaction_on_multi_with_mode(
        &[old_keys::INBOUND_GROUP_SESSIONS_V3, keys::INBOUND_GROUP_SESSIONS_V4],
        IdbTransactionMode::Readwrite,
    )?;

    let inbound_group_sessions3 = txn.object_store(old_keys::INBOUND_GROUP_SESSIONS_V3)?;
    let inbound_group_sessions4 = txn.object_store(keys::INBOUND_GROUP_SESSIONS_V4)?;

    let row_count = inbound_group_sessions3.count()?.await?;
    info!(row_count, "Shrinking inbound_group_session records");

    // Iterate through all rows
    if let Some(cursor) = inbound_group_sessions3.open_cursor()?.await? {
        let mut idx = 0;
        loop {
            idx += 1;

            if idx % 100 == 0 {
                debug!("Migrating session {idx} of {row_count}");
            }

            // Deserialize the session from the old store
            let session = deserialize_inbound_group_session(cursor.value(), serializer)?;

            // Calculate its key in the new table
            let new_key = serializer.encode_key(
                keys::INBOUND_GROUP_SESSIONS_V4,
                (&session.room_id, session.session_id()),
            );

            // Serialize the session in the new format
            let new_session = serialize_inbound_group_session(&session, serializer).await?;

            // Write it to the new store
            inbound_group_sessions4.add_key_val(&new_key, &new_session)?;

            // We are done with the original data, so delete it now.
            cursor.delete()?;

            // Continue to the next record, or stop if we're done
            if !cursor.continue_cursor()?.await? {
                debug!("Migrated {idx} sessions.");
                break;
            }
        }
    }

    // We have finished with the old store. Clear it, since it is faster to
    // clear+delete than just delete. See https://www.artificialworlds.net/blog/2024/02/02/deleting-an-indexed-db-store-can-be-incredibly-slow-on-firefox/
    // for more details.
    inbound_group_sessions3.clear()?.await?;

    txn.await.into_result()?;
    Ok(())
}

/// Perform the schema upgrade v14 to v15, deleting `inbound_group_sessions3`.
pub(crate) async fn schema_delete(name: &str) -> Result<(), DomException> {
    do_schema_upgrade(name, 15, |db, _, _| {
        db.delete_object_store(old_keys::INBOUND_GROUP_SESSIONS_V3)?;
        Ok(())
    })
    .await
}
