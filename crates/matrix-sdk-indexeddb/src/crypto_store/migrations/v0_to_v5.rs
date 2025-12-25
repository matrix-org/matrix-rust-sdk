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

//! Schema-only migrations adding various stores and indices, notably
//! the first version of `inbound_group_sessions`.

use indexed_db_futures::{
    Build,
    database::Database,
    error::{Error, OpenDbError},
};

use crate::crypto_store::{
    Result, keys,
    migrations::{add_nonunique_index, add_unique_index, do_schema_upgrade, old_keys},
};

/// Perform schema migrations as needed, up to schema version 5.
pub(crate) async fn schema_add(name: &str) -> Result<(), OpenDbError> {
    do_schema_upgrade(name, 5, |tx, old_version| {
        let db = tx.db();
        // An old_version of 1 could either mean actually the first version of the
        // schema, or a completely empty schema that has been created with a
        // call to `Database::open` with no explicit "version". So, to determine
        // if we need to create the V1 stores, we actually check if the schema is empty.
        if db.object_store_names().next().is_none() {
            schema_add_v1(db)?;
        }

        if old_version < 2 {
            schema_add_v2(db)?;
        }

        if old_version < 3 {
            schema_add_v3(db)?;
        }

        if old_version < 4 {
            schema_add_v4(db)?;
        }

        if old_version < 5 {
            schema_add_v5(db)?;
        }

        Ok(())
    })
    .await
}

fn schema_add_v1(db: &Database) -> Result<(), Error> {
    db.create_object_store(keys::CORE).build()?;
    db.create_object_store(keys::SESSION).build()?;

    db.create_object_store(old_keys::INBOUND_GROUP_SESSIONS_V1).build()?;
    db.create_object_store(keys::OUTBOUND_GROUP_SESSIONS).build()?;
    db.create_object_store(keys::TRACKED_USERS).build()?;
    db.create_object_store(keys::OLM_HASHES).build()?;
    db.create_object_store(keys::DEVICES).build()?;

    db.create_object_store(keys::IDENTITIES).build()?;
    db.create_object_store(keys::BACKUP_KEYS).build()?;

    Ok(())
}

fn schema_add_v2(db: &Database) -> Result<(), Error> {
    // We changed how we store inbound group sessions, the key used to
    // be a tuple of `(room_id, sender_key, session_id)` now it's a
    // tuple of `(room_id, session_id)`
    //
    // Let's just drop the whole object store.
    db.delete_object_store(old_keys::INBOUND_GROUP_SESSIONS_V1)?;
    db.create_object_store(old_keys::INBOUND_GROUP_SESSIONS_V1).build()?;

    db.create_object_store(keys::ROOM_SETTINGS).build()?;

    Ok(())
}

fn schema_add_v3(db: &Database) -> Result<(), Error> {
    // We changed the way we store outbound session.
    // ShareInfo changed from a struct to an enum with struct variant.
    // Let's just discard the existing outbounds
    db.delete_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;
    db.create_object_store(keys::OUTBOUND_GROUP_SESSIONS).build()?;

    // Support for MSC2399 withheld codes
    db.create_object_store(old_keys::DIRECT_WITHHELD_INFO).build()?;

    Ok(())
}

fn schema_add_v4(db: &Database) -> Result<(), Error> {
    db.create_object_store(keys::SECRETS_INBOX).build()?;
    Ok(())
}

fn schema_add_v5(db: &Database) -> Result<(), Error> {
    // Create a new store for outgoing secret requests
    let object_store = db.create_object_store(keys::GOSSIP_REQUESTS).build()?;

    add_nonunique_index(&object_store, keys::GOSSIP_REQUESTS_UNSENT_INDEX, "unsent")?;

    add_unique_index(&object_store, keys::GOSSIP_REQUESTS_BY_INFO_INDEX, "info")?;

    if db.object_store_names().any(|n| n == "outgoing_secret_requests") {
        // Delete the old store names. We just delete any existing requests.
        db.delete_object_store("outgoing_secret_requests")?;
        db.delete_object_store("unsent_secret_requests")?;
        db.delete_object_store("secret_requests_by_info")?;
    }

    Ok(())
}
