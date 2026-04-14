// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use indexed_db_futures::{
    Build, error::OpenDbError, prelude::QuerySource, transaction::TransactionMode,
};
use matrix_sdk_crypto::GossippedSecret;
use wasm_bindgen::JsValue;

use super::{MigrationDb, old_keys};
use crate::{
    crypto_store::{Result, keys, migrations::do_schema_upgrade},
    serializer::SafeEncodeSerializer,
};

/// Migrate to schema v106: add the new `secrets_inbox2` table.
pub(crate) async fn schema_add(name: &str) -> Result<(), OpenDbError> {
    do_schema_upgrade(name, 106, |tx, _| {
        tx.db().create_object_store(keys::SECRETS_INBOX_V2).build()?;
        Ok(())
    })
    .await
}

/// Migrate the data from different versions of the secrets inbox
pub(crate) async fn data_migrate(name: &str, serializer: &SafeEncodeSerializer) -> Result<()> {
    let db = MigrationDb::new(name, 11).await?;

    // The new store has been made for secrets; time to populate it.
    let txn = db
        .transaction([old_keys::SECRETS_INBOX_V1, keys::SECRETS_INBOX_V2])
        .with_mode(TransactionMode::Readwrite)
        .build()?;

    let old_store = txn.object_store(old_keys::SECRETS_INBOX_V1)?;
    let new_store = txn.object_store(keys::SECRETS_INBOX_V2)?;

    // Iterate through all rows
    if let Some(mut cursor) = old_store.open_cursor().await? {
        while let Some(value) = cursor.next_record::<JsValue>().await? {
            // Deserialize the session from the old store
            let deserialized: GossippedSecret = serializer.deserialize_value(value)?;

            let new_key = serializer.encode_key(
                keys::SECRETS_INBOX_V2,
                (deserialized.secret_name.as_str(), deserialized.event.content.secret.as_str()),
            );
            new_store
                .add(serializer.serialize_value(&deserialized.event.content.secret.as_str())?)
                .with_key(new_key)
                .build()?;
            // We are done with the original data, so delete it now.
            cursor.delete()?;
        }
    }

    txn.commit().await?;
    Ok(())
}

/// Migrate to schema v107: drop the old `secrets_inbox` table.
pub(crate) async fn schema_delete(name: &str) -> Result<(), OpenDbError> {
    do_schema_upgrade(name, 107, |tx, _| {
        tx.db().delete_object_store(old_keys::SECRETS_INBOX_V1)?;
        Ok(())
    })
    .await
}
