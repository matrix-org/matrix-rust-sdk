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

//! Perform the schema upgrade from v104 to v105.
//!
//! Removes the cross-process lock generation counter key from
//! the [`keys::CORE`] object store and then bumps the database
//! version.

use indexed_db_futures::{Build, error::OpenDbError, transaction::TransactionMode};

use super::{MigrationDb, old_keys};
use crate::{
    crypto_store::{Result, keys, migrations::do_schema_upgrade},
    serializer::SafeEncodeSerializer,
};

/// Remove [`GENERATION_COUNTER_KEY`] from [`keys::CORE`] object store, which is
/// no longer used to track cross-process lock generations. The preferred object
/// store for cross-process lock generation tracking is the
/// [`keys::LEASE_LOCKS`] object store.
pub(crate) async fn data_migrate(name: &str, _: &SafeEncodeSerializer) -> Result<()> {
    let db = MigrationDb::new(name, 105).await?;

    let txn = db.transaction([keys::CORE]).with_mode(TransactionMode::Readwrite).build()?;
    txn.object_store(keys::CORE)?.delete(old_keys::GENERATION_COUNTER_KEY).await?;
    txn.commit().await?;

    Ok(())
}

/// Upgrade database without any real schema change, bumping from v104 to v105.
pub(crate) async fn schema_bump(name: &str) -> Result<(), OpenDbError> {
    do_schema_upgrade(name, 105, |_, _| {
        // Bump the version number to 105 to demonstrate that we have
        // made data changes
        Ok(())
    })
    .await
}
