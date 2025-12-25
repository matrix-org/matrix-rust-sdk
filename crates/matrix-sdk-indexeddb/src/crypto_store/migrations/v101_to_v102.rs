// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use indexed_db_futures::{Build, error::OpenDbError};

use crate::crypto_store::{Result, keys, migrations::do_schema_upgrade};

/// Perform the schema upgrade v101 to v102, add the `lease_locks` table.
///
/// Note: it's not trivial to delete old lease locks in the `keys::CORE` table,
/// because we don't know which keys were associated to them. We consider it's
/// fine because it represents a tiny amount of data (maybe 2 rows, with the
/// “`Lease` type” being quite small). To achieve so, we could read all rows in
/// `keys::CORE`, try to deserialize to the `Lease` type, and act accordingly,
/// but each store may have its own `Lease` type. Also note that this
/// `matrix-sdk-indexeddb`
pub(crate) async fn schema_add(name: &str) -> Result<(), OpenDbError> {
    do_schema_upgrade(name, 102, |tx, _| {
        tx.db().create_object_store(keys::LEASE_LOCKS).build()?;
        Ok(())
    })
    .await
}
