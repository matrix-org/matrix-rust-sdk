// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use indexed_db_futures::{prelude::*, web_sys::DomException};
use tracing::info;
use wasm_bindgen::JsValue;

use crate::crypto_store::{keys, Result};

/// Open the indexeddb with the given name, upgrading it to the latest version
/// of the schema if necessary.
pub async fn open_and_upgrade_db(name: &str) -> Result<IdbDatabase, DomException> {
    let mut db_req: OpenDbRequest = IdbDatabase::open_u32(name, 5)?;

    db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        // Even if the web-sys bindings expose the version as a f64, the IndexedDB API
        // works with an unsigned integer.
        // See <https://github.com/rustwasm/wasm-bindgen/issues/1149>
        let old_version = evt.old_version() as u32;
        let new_version = evt.new_version() as u32;

        info!(old_version, new_version, "Upgrading IndexeddbCryptoStore");

        if old_version < 1 {
            create_stores_for_v1(evt.db())?;
        }

        if old_version < 2 {
            create_stores_for_v2(evt.db())?;
        }

        if old_version < 3 {
            create_stores_for_v3(evt.db())?;
        }

        if old_version < 4 {
            create_stores_for_v4(evt.db())?;
        }

        if old_version < 5 {
            create_stores_for_v5(evt.db())?;
        }

        info!(old_version, new_version, "IndexeddbCryptoStore upgrade complete");
        Ok(())
    }));

    db_req.await
}

fn create_stores_for_v1(db: &IdbDatabase) -> Result<(), DomException> {
    db.create_object_store(keys::CORE)?;
    db.create_object_store(keys::SESSION)?;

    db.create_object_store(keys::INBOUND_GROUP_SESSIONS)?;
    db.create_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;
    db.create_object_store(keys::TRACKED_USERS)?;
    db.create_object_store(keys::OLM_HASHES)?;
    db.create_object_store(keys::DEVICES)?;

    db.create_object_store(keys::IDENTITIES)?;
    db.create_object_store(keys::BACKUP_KEYS)?;

    Ok(())
}

fn create_stores_for_v2(db: &IdbDatabase) -> Result<(), DomException> {
    // We changed how we store inbound group sessions, the key used to
    // be a tuple of `(room_id, sender_key, session_id)` now it's a
    // tuple of `(room_id, session_id)`
    //
    // Let's just drop the whole object store.
    db.delete_object_store(keys::INBOUND_GROUP_SESSIONS)?;
    db.create_object_store(keys::INBOUND_GROUP_SESSIONS)?;

    db.create_object_store(keys::ROOM_SETTINGS)?;

    Ok(())
}

fn create_stores_for_v3(db: &IdbDatabase) -> Result<(), DomException> {
    // We changed the way we store outbound session.
    // ShareInfo changed from a struct to an enum with struct variant.
    // Let's just discard the existing outbounds
    db.delete_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;
    db.create_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;

    // Support for MSC2399 withheld codes
    db.create_object_store(keys::DIRECT_WITHHELD_INFO)?;

    Ok(())
}

fn create_stores_for_v4(db: &IdbDatabase) -> Result<(), DomException> {
    db.create_object_store(keys::SECRETS_INBOX)?;
    Ok(())
}

fn create_stores_for_v5(db: &IdbDatabase) -> Result<(), DomException> {
    // Create a new store for outgoing secret requests
    let object_store = db.create_object_store(keys::GOSSIP_REQUESTS)?;

    let mut params = IdbIndexParameters::new();
    params.unique(false);
    object_store.create_index_with_params(
        keys::GOSSIP_REQUESTS_UNSENT_INDEX,
        &IdbKeyPath::str("unsent"),
        &params,
    )?;

    let mut params = IdbIndexParameters::new();
    params.unique(true);
    object_store.create_index_with_params(
        keys::GOSSIP_REQUESTS_BY_INFO_INDEX,
        &IdbKeyPath::str("info"),
        &params,
    )?;

    if db.object_store_names().any(|n| n == "outgoing_secret_requests") {
        // Delete the old store names. We just delete any existing requests.
        db.delete_object_store("outgoing_secret_requests")?;
        db.delete_object_store("unsent_secret_requests")?;
        db.delete_object_store("secret_requests_by_info")?;
    }

    Ok(())
}
