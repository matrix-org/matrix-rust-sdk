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

use indexed_db_futures::{Build, error::OpenDbError, transaction::TransactionMode};

use super::MigrationDb;
use crate::{
    crypto_store::{Result, keys, migrations::do_schema_upgrade},
    serializer::SafeEncodeSerializer,
};

pub(crate) async fn data_migrate(name: &str, _: &SafeEncodeSerializer) -> Result<()> {
    let db = MigrationDb::new(name, 14).await?;
    let transaction = db
        .transaction(keys::RECEIVED_ROOM_KEY_BUNDLES)
        .with_mode(TransactionMode::Readwrite)
        .build()?;
    let store = transaction.object_store(keys::RECEIVED_ROOM_KEY_BUNDLES)?;

    // The schema didn't actually change, we just changed the objects that are
    // stored. So let us remove them.
    store.clear()?;

    transaction.commit().await?;

    Ok(())
}

/// Perform the schema upgrade v13 to v14, just bumping the schema version since
/// the schema didn't actually change.
pub(crate) async fn schema_bump(name: &str) -> Result<(), OpenDbError> {
    // Just bump the version number to 14 to demonstrate that we have run the data
    // changes from data_migrate.
    do_schema_upgrade(name, 14, |_, _| Ok(())).await
}
