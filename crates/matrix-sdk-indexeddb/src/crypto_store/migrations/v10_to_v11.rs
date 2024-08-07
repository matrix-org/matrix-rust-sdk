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

//! Migration code that moves from `backup_keys.backup_key_v1` to
//! `backup_keys.backup_version_v1`, switching to a new serialization format.

use indexed_db_futures::IdbQuerySource;
use wasm_bindgen::JsValue;
use web_sys::{DomException, IdbTransactionMode};

use crate::crypto_store::{
    indexeddb_serializer::IndexeddbSerializer,
    keys,
    migrations::{do_schema_upgrade, old_keys, MigrationDb},
};

/// Migrate data from `backup_keys.backup_key_v1` to
/// `backup_keys.backup_version_v1`.
pub(crate) async fn data_migrate(
    name: &str,
    serializer: &IndexeddbSerializer,
) -> crate::crypto_store::Result<()> {
    let db = MigrationDb::new(name, 11).await?;
    let txn = db.transaction_on_one_with_mode(keys::BACKUP_KEYS, IdbTransactionMode::Readwrite)?;
    let store = txn.object_store(keys::BACKUP_KEYS)?;

    let bv = store.get(&JsValue::from_str(old_keys::BACKUP_KEY_V1))?.await?;

    let Some(bv) = bv else {
        return Ok(());
    };

    // backup_key_v1 was only ever serialized with the legacy format. Also, it's a
    // string, so if we use `deserialize_value` on it, it will be incorrectly
    // handled as a new-format object.
    let bv: String = serializer.deserialize_legacy_value(bv)?;

    // Re-serialize as new format, then store in the new field.
    let serialized = serializer.serialize_value(&bv)?;
    store.put_key_val(&JsValue::from_str(keys::BACKUP_VERSION_V1), &serialized)?.await?;
    store.delete(&JsValue::from_str(old_keys::BACKUP_KEY_V1))?.await?;
    Ok(())
}

/// Perform the schema upgrade v10 to v11, just bumping the schema version.
pub(crate) async fn schema_bump(name: &str) -> crate::crypto_store::Result<(), DomException> {
    // Just bump the version number to 11 to demonstrate that we have run the data
    // changes from data_migrate.
    do_schema_upgrade(name, 11, |_, _, _| Ok(())).await
}
