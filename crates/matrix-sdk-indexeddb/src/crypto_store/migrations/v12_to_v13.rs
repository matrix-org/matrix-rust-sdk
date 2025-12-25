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

use indexed_db_futures::{Build, error::OpenDbError};

use crate::crypto_store::{Result, keys, migrations::do_schema_upgrade};

/// Perform the schema upgrade v12 to v13, adding the
/// `received_room_key_bundles` store.
pub(crate) async fn schema_add(name: &str) -> Result<(), OpenDbError> {
    do_schema_upgrade(name, 13, |tx, _| {
        tx.db().create_object_store(keys::RECEIVED_ROOM_KEY_BUNDLES).build()?;
        Ok(())
    })
    .await
}
