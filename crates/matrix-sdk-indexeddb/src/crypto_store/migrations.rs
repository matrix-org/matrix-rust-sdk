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
use matrix_sdk_crypto::olm::InboundGroupSession;
use tracing::{debug, info};
use wasm_bindgen::JsValue;

use crate::{
    crypto_store::{
        indexeddb_serializer::IndexeddbSerializer, keys, InboundGroupSessionIndexedDbObject, Result,
    },
    IndexeddbCryptoStoreError,
};

mod old_keys {
    /// Old format of the inbound_group_sessions store which lacked indexes or a
    /// sensible structure
    pub const INBOUND_GROUP_SESSIONS_V1: &str = "inbound_group_sessions";
}

/// Open the indexeddb with the given name, upgrading it to the latest version
/// of the schema if necessary.
pub async fn open_and_upgrade_db(
    name: &str,
    serializer: &IndexeddbSerializer,
) -> Result<IdbDatabase, IndexeddbCryptoStoreError> {
    // This is all a bit of a hack. Some of the version migrations require a data
    // migration, which has to be done via async APIs; however, the
    // JS `upgrade_needed` mechanism does not allow for async calls.
    //
    // Start by finding out what the existing version is, if any.
    let db = IdbDatabase::open(name)?.await?;
    let old_version = db.version() as u32;
    db.close();

    // If we have yet to complete the migration to V7, migrate the schema to V6
    // (if necessary), and then migrate any remaining data.
    if old_version <= 6 {
        let db = migrate_schema_up_to_v6(name).await?;
        migrate_data_for_v6(serializer, &db).await?;
        db.close();
    }

    // Now we can safely complete the migration to V7 which will drop the old store.
    let mut db_req: OpenDbRequest = IdbDatabase::open_u32(name, 7)?;
    db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        let old_version = evt.old_version() as u32;
        let new_version = evt.new_version() as u32;

        info!(old_version, new_version, "Continuing IndexeddbCryptoStore upgrade");

        if old_version < 7 {
            migrate_stores_to_v7(evt.db())?;
        }

        info!(old_version, new_version, "IndexeddbCryptoStore upgrade complete");
        Ok(())
    }));

    Ok(db_req.await?)
}

async fn migrate_schema_up_to_v6(name: &str) -> Result<IdbDatabase, DomException> {
    let mut db_req: OpenDbRequest = IdbDatabase::open_u32(name, 6)?;

    db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        // Even if the web-sys bindings expose the version as a f64, the IndexedDB API
        // works with an unsigned integer.
        // See <https://github.com/rustwasm/wasm-bindgen/issues/1149>
        let old_version = evt.old_version() as u32;
        let new_version = evt.new_version() as u32;

        info!(old_version, new_version, "Upgrading IndexeddbCryptoStore, phase 1");

        // An old_version of 1 could either mean actually the first version of the
        // schema, or a completely empty schema that has been created with a
        // call to `IdbDatabase::open` with no explicit "version". So, to determine
        // if we need to create the V1 stores, we actually check if the schema is empty.
        if evt.db().object_store_names().next().is_none() {
            migrate_stores_to_v1(evt.db())?;
        }

        if old_version < 2 {
            migrate_stores_to_v2(evt.db())?;
        }

        if old_version < 3 {
            migrate_stores_to_v3(evt.db())?;
        }

        if old_version < 4 {
            migrate_stores_to_v4(evt.db())?;
        }

        if old_version < 5 {
            migrate_stores_to_v5(evt.db())?;
        }

        if old_version < 6 {
            migrate_stores_to_v6(evt.db())?;
        }

        // NOTE! Further migrations must NOT be added here.
        //
        // At this point we need to start an asynchronous operation to migrate
        // inbound_group_sessions to a new format. We then resume schema migrations
        // afterwards.
        //
        // Further migrations can be added in `open_and_upgrade_db`.

        info!(old_version, new_version, "IndexeddbCryptoStore upgrade phase 1 complete");
        Ok(())
    }));

    db_req.await
}

fn migrate_stores_to_v1(db: &IdbDatabase) -> Result<(), DomException> {
    db.create_object_store(keys::CORE)?;
    db.create_object_store(keys::SESSION)?;

    db.create_object_store(old_keys::INBOUND_GROUP_SESSIONS_V1)?;
    db.create_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;
    db.create_object_store(keys::TRACKED_USERS)?;
    db.create_object_store(keys::OLM_HASHES)?;
    db.create_object_store(keys::DEVICES)?;

    db.create_object_store(keys::IDENTITIES)?;
    db.create_object_store(keys::BACKUP_KEYS)?;

    Ok(())
}

fn migrate_stores_to_v2(db: &IdbDatabase) -> Result<(), DomException> {
    // We changed how we store inbound group sessions, the key used to
    // be a tuple of `(room_id, sender_key, session_id)` now it's a
    // tuple of `(room_id, session_id)`
    //
    // Let's just drop the whole object store.
    db.delete_object_store(old_keys::INBOUND_GROUP_SESSIONS_V1)?;
    db.create_object_store(old_keys::INBOUND_GROUP_SESSIONS_V1)?;

    db.create_object_store(keys::ROOM_SETTINGS)?;

    Ok(())
}

fn migrate_stores_to_v3(db: &IdbDatabase) -> Result<(), DomException> {
    // We changed the way we store outbound session.
    // ShareInfo changed from a struct to an enum with struct variant.
    // Let's just discard the existing outbounds
    db.delete_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;
    db.create_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;

    // Support for MSC2399 withheld codes
    db.create_object_store(keys::DIRECT_WITHHELD_INFO)?;

    Ok(())
}

fn migrate_stores_to_v4(db: &IdbDatabase) -> Result<(), DomException> {
    db.create_object_store(keys::SECRETS_INBOX)?;
    Ok(())
}

fn migrate_stores_to_v5(db: &IdbDatabase) -> Result<(), DomException> {
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

fn migrate_stores_to_v6(db: &IdbDatabase) -> Result<(), DomException> {
    // We want to change the shape of the inbound group sessions store. To do so, we
    // first need to build a new store, then copy all the data over.
    //
    // But copying the data needs to happen outside the database upgrade process
    // (because it needs async calls). So, here we create a new store for
    // inbound group sessions. We don't populate it yet; that happens once we
    // have done the upgrade to v6, in `migrate_data_for_v6`. Finally we drop the
    // old store in create_stores_for_v7.

    let object_store = db.create_object_store(keys::INBOUND_GROUP_SESSIONS_V2)?;

    let mut params = IdbIndexParameters::new();
    params.unique(false);
    object_store.create_index_with_params(
        keys::INBOUND_GROUP_SESSIONS_BACKUP_INDEX,
        &IdbKeyPath::str("needs_backup"),
        &params,
    )?;

    Ok(())
}

async fn migrate_data_for_v6(serializer: &IndexeddbSerializer, db: &IdbDatabase) -> Result<()> {
    // The new store has been made for inbound group sessions; time to populate it.
    let txn = db.transaction_on_multi_with_mode(
        &[old_keys::INBOUND_GROUP_SESSIONS_V1, keys::INBOUND_GROUP_SESSIONS_V2],
        IdbTransactionMode::Readwrite,
    )?;

    let old_store = txn.object_store(old_keys::INBOUND_GROUP_SESSIONS_V1)?;
    let new_store = txn.object_store(keys::INBOUND_GROUP_SESSIONS_V2)?;

    let row_count = old_store.count()?.await?;
    info!(row_count, "Migrating inbound group session data from v1 to v2");

    if let Some(cursor) = old_store.open_cursor()?.await? {
        let mut idx = 0;
        loop {
            idx += 1;
            let key = cursor.key().ok_or(matrix_sdk_crypto::CryptoStoreError::Backend(
                "inbound_group_sessions v1 cursor has no key".into(),
            ))?;
            let value = cursor.value();

            if idx % 100 == 0 {
                debug!("Migrating session {idx} of {row_count}");
            }

            let igs = InboundGroupSession::from_pickle(serializer.deserialize_value(value)?)
                .map_err(|e| IndexeddbCryptoStoreError::CryptoStoreError(e.into()))?;

            // This is much the same as `IndexeddbStore::serialize_inbound_group_session`.
            let new_data = serde_wasm_bindgen::to_value(&InboundGroupSessionIndexedDbObject {
                pickled_session: serializer.serialize_value_as_bytes(&igs.pickle().await)?,
                needs_backup: !igs.backed_up(),
            })?;

            new_store.add_key_val(&key, &new_data)?;

            // we are done with the original data, so delete it now.
            cursor.delete()?;

            if !cursor.continue_cursor()?.await? {
                break;
            }
        }
    }

    Ok(txn.await.into_result()?)
}

fn migrate_stores_to_v7(db: &IdbDatabase) -> Result<(), DomException> {
    db.delete_object_store(old_keys::INBOUND_GROUP_SESSIONS_V1)
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use std::sync::Arc;

    use indexed_db_futures::prelude::*;
    use matrix_sdk_common::js_tracing::make_tracing_subscriber;
    use matrix_sdk_crypto::{
        olm::SessionKey,
        store::CryptoStore,
        types::EventEncryptionAlgorithm,
        vodozemac::{Curve25519PublicKey, Curve25519SecretKey, Ed25519SecretKey},
    };
    use matrix_sdk_store_encryption::StoreCipher;
    use matrix_sdk_test::async_test;
    use ruma::room_id;
    use tracing_subscriber::util::SubscriberInitExt;

    use crate::{crypto_store::migrations::*, IndexeddbCryptoStore};

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    /// Test migrating `inbound_group_session` data from store v5 to store v7,
    /// on a store with encryption disabled.
    #[async_test]
    async fn test_v7_migration_unencrypted() {
        test_v7_migration_with_cipher("test_v7_migration_unencrypted", None).await
    }

    /// Test migrating `inbound_group_session` data from store v5 to store v7,
    /// on a store with encryption enabled.
    #[async_test]
    async fn test_v7_migration_encrypted() {
        let cipher = StoreCipher::new().unwrap();
        test_v7_migration_with_cipher("test_v7_migration_encrypted", Some(Arc::new(cipher))).await;
    }

    /// Helper function for `test_v7_migration_{un,}encrypted`: test migrating
    /// `inbound_group_session` data from store v5 to store v7.
    async fn test_v7_migration_with_cipher(
        db_prefix: &str,
        store_cipher: Option<Arc<StoreCipher>>,
    ) {
        let _ = make_tracing_subscriber(None).try_init();
        let db_name = format!("{db_prefix:0}::matrix-sdk-crypto");

        // delete the db in case it was used in a previous run
        let _ = IdbDatabase::delete_by_name(&db_name);

        // Schema V7 migrated the inbound group sessions to a new format.
        // To test, first create a database and populate it with the *old* style of
        // entry.
        let db = create_v5_db(&db_name).await.unwrap();

        let room_id = room_id!("!test:localhost");
        let curve_key = Curve25519PublicKey::from(&Curve25519SecretKey::new());
        let ed_key = Ed25519SecretKey::new().public_key();

        // a backed-up session
        let session1 = InboundGroupSession::new(
            curve_key,
            ed_key,
            room_id,
            &SessionKey::from_base64(
                "AgAAAABTyn3CR8mzAxhsHH88td5DrRqfipJCnNbZeMrfzhON6O1Cyr9ewx/sDFLO6\
                 +NvyW92yGvMub7nuAEQb+SgnZLm7nwvuVvJgSZKpoJMVliwg8iY9TXKFT286oBtT2\
                 /8idy6TcpKax4foSHdMYlZXu5zOsGDdd9eYnYHpUEyDT0utuiaakZM3XBMNLEVDj9\
                 Ps929j1FGgne1bDeFVoty2UAOQK8s/0JJigbKSu6wQ/SzaCYpE/LD4Egk2Nxs1JE2\
                 33ii9J8RGPYOp7QWl0kTEc8mAlqZL7mKppo9AwgtmYweAg",
            )
            .unwrap(),
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
        )
        .unwrap();
        session1.mark_as_backed_up();

        // an un-backed-up session
        let session2 = InboundGroupSession::new(
            curve_key,
            ed_key,
            room_id,
            &SessionKey::from_base64(
                "AgAAAACO1PjBdqucFUcNFU6JgXYAi7KMeeUqUibaLm6CkHJcMiDTFWq/K5SFAukJc\
                 WjeyOpnZr4vpezRlbvNaQpNPMub2Cs2u14fHj9OpKFD7c4hFS4j94q4pTLZly3qEV\
                 BIjWdOpcIVfN7QVGVIxYiI6KHEddCHrNCo9fc8GUdfzrMnmUooQr/m4ZAkRdErzUH\
                 uUAlUBwOKcPi7Cs/KrMw/sHCRDkTntHZ3BOrzJsAVbHUgq+8/Sqy3YE+CX6uEnig+\
                 1NWjZD9f1vvXnSKKDdHj1927WFMFZ/yYc24607zEVUaODQ",
            )
            .unwrap(),
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
        )
        .unwrap();

        let serializer = IndexeddbSerializer::new(store_cipher.clone());

        let txn = db
            .transaction_on_one_with_mode(
                old_keys::INBOUND_GROUP_SESSIONS_V1,
                IdbTransactionMode::Readwrite,
            )
            .unwrap();
        let sessions = txn.object_store(old_keys::INBOUND_GROUP_SESSIONS_V1).unwrap();
        for session in vec![&session1, &session2] {
            let room_id = session.room_id();
            let session_id = session.session_id();
            let key = serializer.encode_key(keys::INBOUND_GROUP_SESSIONS_V2, (room_id, session_id));
            let pickle = session.pickle().await;

            sessions.put_key_val(&key, &serializer.serialize_value(&pickle).unwrap()).unwrap();
        }
        txn.await.into_result().unwrap();

        // now close our DB, reopen it properly, and check that we can still read our
        // data.
        db.close();

        let store =
            IndexeddbCryptoStore::open_with_store_cipher(&db_prefix, store_cipher).await.unwrap();

        let s =
            store.get_inbound_group_session(room_id, session1.session_id()).await.unwrap().unwrap();
        assert_eq!(s.session_id(), session1.session_id());
        assert_eq!(s.backed_up(), true);

        let s =
            store.get_inbound_group_session(room_id, session2.session_id()).await.unwrap().unwrap();
        assert_eq!(s.session_id(), session2.session_id());
        assert_eq!(s.backed_up(), false);
    }

    async fn create_v5_db(name: &str) -> std::result::Result<IdbDatabase, DomException> {
        let mut db_req: OpenDbRequest = IdbDatabase::open_u32(name, 5)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            let db = evt.db();
            migrate_stores_to_v1(db)?;
            migrate_stores_to_v2(db)?;
            migrate_stores_to_v3(db)?;
            migrate_stores_to_v4(db)?;
            migrate_stores_to_v5(db)?;
            Ok(())
        }));
        db_req.await
    }
}
