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

use std::ops::Deref;

use indexed_db_futures::{
    database::Database,
    error::{Error, OpenDbError},
    index::Index,
    internals::SystemRepr,
    object_store::ObjectStore,
    prelude::*,
    transaction::Transaction,
};
use tracing::info;

use crate::{IndexeddbCryptoStoreError, crypto_store::Result, serializer::SafeEncodeSerializer};

mod old_keys;
mod v0_to_v5;
mod v101_to_v102;
mod v102_to_v103;
mod v10_to_v11;
mod v11_to_v12;
mod v12_to_v13;
mod v13_to_v14;
mod v14_to_v101;
mod v5_to_v7;
mod v7;
mod v7_to_v8;
mod v8_to_v10;

struct MigrationDb {
    db: Database,
    next_version: u32,
}

impl MigrationDb {
    /// Create an Indexed DB wrapper that manages a database migration,
    /// logging messages before and after the migration, and automatically
    /// closing the DB when this object is dropped.
    async fn new(name: &str, next_version: u32) -> Result<Self> {
        info!("IndexeddbCryptoStore migrate data before v{next_version} starting");
        Ok(Self { db: Database::open(name).await?, next_version })
    }
}

impl Deref for MigrationDb {
    type Target = Database;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl Drop for MigrationDb {
    fn drop(&mut self) {
        let version = self.next_version;
        info!("IndexeddbCryptoStore migrate data before v{version} finished");
        self.db.as_sys().close();
    }
}

/// The latest version of the schema we can support. If we encounter a database
/// version with a higher schema version, we will return an error.
///
/// A note on how this works.
///
/// Normally, when you open an indexeddb database, you tell it the "schema
/// version" that you know about. If the existing database is older than
/// that, it lets you run a migration. If the existing database is newer, then
/// it assumes that there have been incompatible schema changes and complains
/// with an error ("The requested version (10) is less than the existing version
/// (11)").
///
/// The problem with this is that, if someone upgrades their installed
/// application, then realises it was a terrible mistake and tries to roll
/// back, then suddenly every user's session is completely hosed. (They see
/// an "unable to restore session" dialog.) Often, schema updates aren't
/// actually backwards-incompatible — for example, existing code will work just
/// fine if someone adds a new store or a new index — so this approach is too
/// heavy-handed.
///
/// The solution we take here is to say "any schema changes up to
/// [`MAX_SUPPORTED_SCHEMA_VERSION`] will be backwards-compatible". If, at some
/// point, we do make a breaking change, we will give that schema version a
/// higher number. Then, rather than using the implicit version check that comes
/// with `indexedDB.open(name, version)`, we explicitly check the version
/// ourselves.
///
/// It is expected that we will use version numbers that are multiples of 100 to
/// represent breaking changes — for example, version 100 is a breaking change,
/// as is version 200, but versions 101-199 are all backwards compatible with
/// version 100. In other words, if you divide by 100, you get something
/// approaching semver: version 200 is major version 2, minor version 0.
const MAX_SUPPORTED_SCHEMA_VERSION: u32 = 199;

/// Open the indexeddb with the given name, upgrading it to the latest version
/// of the schema if necessary.
pub async fn open_and_upgrade_db(
    name: &str,
    serializer: &SafeEncodeSerializer,
) -> Result<Database, IndexeddbCryptoStoreError> {
    // Move the DB version up from where it is to the latest version.
    //
    // Schema changes need to be separate from data migrations, so we often
    // have a pattern of:
    //
    // 1. schema_add - create new object stores, indices etc.
    // 2. data_migrate - move data from the old stores to the new ones
    // 3. schema_delete - delete any now-unused stores etc.
    //
    // Migrations like these require the schema version to be bumped twice,
    // because of the separate "add" and "delete" stages.

    let old_version = db_version(name).await?;

    // If the database version is too new, bail out. We assume that schema updates
    // all the way up to `MAX_SUPPORTED_SCHEMA_VERSION` will be
    // backwards-compatible.
    if old_version > MAX_SUPPORTED_SCHEMA_VERSION {
        return Err(IndexeddbCryptoStoreError::SchemaTooNewError {
            max_supported_version: MAX_SUPPORTED_SCHEMA_VERSION,
            current_version: old_version,
        });
    }

    if old_version < 5 {
        v0_to_v5::schema_add(name).await?;
    }

    if old_version < 6 {
        v5_to_v7::schema_add(name).await?;
    }
    if old_version < 7 {
        v5_to_v7::data_migrate(name, serializer).await?;
        v5_to_v7::schema_delete(name).await?;
    }

    if old_version < 8 {
        v7_to_v8::data_migrate(name, serializer).await?;
        v7_to_v8::schema_bump(name).await?;
    }

    if old_version < 9 {
        v8_to_v10::schema_add(name).await?;
    }
    if old_version < 10 {
        v8_to_v10::data_migrate(name, serializer).await?;
        v8_to_v10::schema_delete(name).await?;
    }

    if old_version < 11 {
        v10_to_v11::data_migrate(name, serializer).await?;
        v10_to_v11::schema_bump(name).await?;
    }

    if old_version < 12 {
        v11_to_v12::schema_add(name).await?;
    }

    if old_version < 13 {
        v12_to_v13::schema_add(name).await?;
    }

    if old_version < 14 {
        v13_to_v14::data_migrate(name, serializer).await?;
        v13_to_v14::schema_bump(name).await?;
    }

    if old_version < 100 {
        v14_to_v101::schema_add(name).await?;
    }

    if old_version < 101 {
        v14_to_v101::data_migrate(name, serializer).await?;
        v14_to_v101::schema_delete(name).await?;
    }

    if old_version < 102 {
        v101_to_v102::schema_add(name).await?;
    }

    if old_version < 103 {
        v102_to_v103::schema_add(name).await?;
    }

    // If you add more migrations here, you'll need to update
    // `tests::EXPECTED_SCHEMA_VERSION`.

    // NOTE: IF YOU MAKE A BREAKING CHANGE TO THE SCHEMA, BUMP THE SCHEMA VERSION TO
    // SOMETHING HIGHER THAN `MAX_SUPPORTED_SCHEMA_VERSION`! (And then bump
    // `MAX_SUPPORTED_SCHEMA_VERSION` itself to the next multiple of 10).

    // Open and return the DB (we know it's at the latest version)
    Ok(Database::open(name).await?)
}

async fn db_version(name: &str) -> Result<u32, IndexeddbCryptoStoreError> {
    let db = Database::open(name).await?;
    let old_version = db.version() as u32;
    db.close();
    Ok(old_version)
}

type OldVersion = u32;

/// Run a database schema upgrade operation
///
/// # Arguments
///
/// * `name` - name of the indexeddb database to be upgraded.
/// * `version` - version we are upgrading to.
/// * `f` - closure which will be called if the database is below the version
///   given. It will be called with three arguments `(db, txn, oldver)`, where:
///   * `db` - the [`Database`]
///   * `txn` - the database transaction: a [`Transaction`]
///   * `oldver` - the version number before the upgrade.
async fn do_schema_upgrade<F>(name: &str, version: u32, f: F) -> Result<(), OpenDbError>
where
    F: Fn(&Transaction<'_>, OldVersion) -> Result<(), Error> + 'static,
{
    info!("IndexeddbCryptoStore upgrade schema -> v{version} starting");
    let db = Database::open(name)
        .with_version(version)
        .with_on_upgrade_needed(move |evt, tx| {
            // Even if the web-sys bindings expose the version as a f64, the IndexedDB API
            // works with an unsigned integer.
            // See <https://github.com/rustwasm/wasm-bindgen/issues/1149>
            let old_version = evt.old_version() as u32;

            // Run the upgrade code we were supplied
            f(tx, old_version)
        })
        .await?;
    db.close();
    info!("IndexeddbCryptoStore upgrade schema -> v{version} complete");
    Ok(())
}

fn add_nonunique_index<'a>(
    object_store: &'a ObjectStore<'a>,
    name: &str,
    key_path: &str,
) -> Result<Index<'a>, Error> {
    object_store.create_index(name, key_path.into()).with_unique(false).build()
}

fn add_unique_index<'a>(
    object_store: &'a ObjectStore<'a>,
    name: &str,
    key_path: &str,
) -> Result<Index<'a>, Error> {
    object_store.create_index(name, key_path.into()).with_unique(true).build()
}

#[cfg(all(test, target_family = "wasm"))]
mod tests {
    use std::{cell::Cell, future::Future, rc::Rc, sync::Arc};

    use assert_matches::assert_matches;
    use gloo_utils::format::JsValueSerdeExt;
    use indexed_db_futures::{
        database::VersionChangeEvent, prelude::*, transaction::TransactionMode,
    };
    use matrix_sdk_common::{
        deserialized_responses::WithheldCode, js_tracing::make_tracing_subscriber,
    };
    use matrix_sdk_crypto::{
        olm::{InboundGroupSession, SenderData, SessionKey},
        store::{CryptoStore, types::RoomKeyWithheldEntry},
        types::{EventEncryptionAlgorithm, events::room_key_withheld::RoomKeyWithheldContent},
        vodozemac::{Curve25519PublicKey, Curve25519SecretKey, Ed25519PublicKey, Ed25519SecretKey},
    };
    use matrix_sdk_store_encryption::StoreCipher;
    use matrix_sdk_test::async_test;
    use ruma::{OwnedRoomId, RoomId, device_id, owned_user_id, room_id};
    use serde::Serialize;
    use tracing_subscriber::util::SubscriberInitExt;
    use wasm_bindgen::JsValue;
    use web_sys::console;

    use super::{v0_to_v5, v7::InboundGroupSessionIndexedDbObject2};
    use crate::{
        IndexeddbCryptoStore,
        crypto_store::{InboundGroupSessionIndexedDbObject, keys, migrations::*},
    };

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    /// The schema version we expect after we open the store.
    const EXPECTED_SCHEMA_VERSION: u32 = 103;

    /// Adjust this to test do a more comprehensive perf test
    const NUM_RECORDS_FOR_PERF: usize = 2_000;

    /// Make lots of sessions and see how long it takes to count them in v8
    #[async_test]
    async fn test_count_lots_of_sessions_v8() {
        let cipher = Arc::new(StoreCipher::new().unwrap());
        let serializer = SafeEncodeSerializer::new(Some(cipher.clone()));
        // Session keys are slow to create, so make one upfront and use it for every
        // session
        let session_key = create_session_key();

        // Create lots of InboundGroupSessionIndexedDbObject2 objects
        let mut objects = Vec::with_capacity(NUM_RECORDS_FOR_PERF);
        for i in 0..NUM_RECORDS_FOR_PERF {
            objects.push(
                create_inbound_group_sessions2_record(i, &session_key, &cipher, &serializer).await,
            );
        }

        // Create a DB with an inbound_group_sessions2 store
        let db_prefix = "count_lots_of_sessions_v8";
        let db = create_db(db_prefix).await;
        let transaction = create_transaction(&db, db_prefix).await;
        let store = create_store(&transaction, db_prefix).await;

        // Check how long it takes to insert these records
        measure_performance("Inserting", "v8", NUM_RECORDS_FOR_PERF, || async {
            for (key, session_js) in objects.iter() {
                store
                    .add(session_js)
                    .with_key(key)
                    .without_key_type()
                    .build()
                    .unwrap()
                    .await
                    .unwrap();
            }
        })
        .await;

        // Check how long it takes to count these records
        measure_performance("Counting", "v8", NUM_RECORDS_FOR_PERF, || async {
            store.count().await.unwrap();
        })
        .await;
    }

    /// Make lots of sessions and see how long it takes to count them in v10
    #[async_test]
    async fn test_count_lots_of_sessions_v10() {
        let serializer = SafeEncodeSerializer::new(Some(Arc::new(StoreCipher::new().unwrap())));

        // Session keys are slow to create, so make one upfront and use it for every
        // session
        let session_key = create_session_key();

        // Create lots of InboundGroupSessionIndexedDbObject objects
        let mut objects = Vec::with_capacity(NUM_RECORDS_FOR_PERF);
        for i in 0..NUM_RECORDS_FOR_PERF {
            objects.push(create_inbound_group_sessions3_record(i, &session_key, &serializer).await);
        }

        // Create a DB with an inbound_group_sessions3 store
        let db_prefix = "count_lots_of_sessions_v8";
        let db = create_db(db_prefix).await;
        let transaction = create_transaction(&db, db_prefix).await;
        let store = create_store(&transaction, db_prefix).await;

        // Check how long it takes to insert these records
        measure_performance("Inserting", "v10", NUM_RECORDS_FOR_PERF, || async {
            for (key, session_js) in objects.iter() {
                store
                    .add(session_js)
                    .with_key(key)
                    .without_key_type()
                    .build()
                    .unwrap()
                    .await
                    .unwrap();
            }
        })
        .await;

        // Check how long it takes to count these records
        measure_performance("Counting", "v10", NUM_RECORDS_FOR_PERF, || async {
            store.count().await.unwrap();
        })
        .await;
    }

    async fn create_db(db_prefix: &str) -> Database {
        let db_name = format!("{db_prefix}::matrix-sdk-crypto");
        let store_name = format!("{db_prefix}_store");
        Database::open(&db_name)
            .with_version(1u32)
            .with_on_upgrade_needed(
                move |_: VersionChangeEvent, tx: &Transaction<'_>| -> Result<(), Error> {
                    tx.db().create_object_store(&store_name).build()?;
                    Ok(())
                },
            )
            .build()
            .unwrap()
            .await
            .unwrap()
    }

    async fn create_transaction<'a>(db: &'a Database, db_prefix: &str) -> Transaction<'a> {
        let store_name = format!("{db_prefix}_store");
        db.transaction(&store_name).with_mode(TransactionMode::Readwrite).build().unwrap()
    }

    async fn create_store<'a>(
        transaction: &'a Transaction<'a>,
        db_prefix: &str,
    ) -> ObjectStore<'a> {
        let store_name = format!("{db_prefix}_store");
        transaction.object_store(&store_name).unwrap()
    }

    fn create_session_key() -> SessionKey {
        SessionKey::from_base64(
            "\
            AgAAAADBy9+YIYTIqBjFT67nyi31gIOypZQl8day2hkhRDCZaHoG+cZh4tZLQIAZimJail0\
            0zq4DVJVljO6cZ2t8kIto/QVk+7p20Fcf2nvqZyL2ZCda2Ei7VsqWZHTM/gqa2IU9+ktkwz\
            +KFhENnHvDhG9f+hjsAPZd5mTTpdO+tVcqtdWhX4dymaJ/2UpAAjuPXQW+nXhQWQhXgXOUa\
            JCYurJtvbCbqZGeDMmVIoqukBs2KugNJ6j5WlTPoeFnMl6Guy9uH2iWWxGg8ZgT2xspqVl5\
            CwujjC+m7Dh1toVkvu+bAw\
            ",
        )
        .unwrap()
    }

    async fn create_inbound_group_sessions2_record(
        i: usize,
        session_key: &SessionKey,
        cipher: &Arc<StoreCipher>,
        serializer: &SafeEncodeSerializer,
    ) -> (JsValue, JsValue) {
        let session = create_inbound_group_session(i, session_key);
        let pickled_session = session.pickle().await;
        let session_dbo = InboundGroupSessionIndexedDbObject2 {
            pickled_session: cipher.encrypt_value(&pickled_session).unwrap(),
            needs_backup: false,
        };
        let session_js: JsValue = serde_wasm_bindgen::to_value(&session_dbo).unwrap();

        let key = serializer.encode_key(
            old_keys::INBOUND_GROUP_SESSIONS_V2,
            (&session.room_id, session.session_id()),
        );

        (key, session_js)
    }

    async fn create_inbound_group_sessions3_record(
        i: usize,
        session_key: &SessionKey,
        serializer: &SafeEncodeSerializer,
    ) -> (JsValue, JsValue) {
        let session = create_inbound_group_session(i, session_key);
        let pickled_session = session.pickle().await;

        let session_dbo = InboundGroupSessionIndexedDbObject {
            pickled_session: serializer.maybe_encrypt_value(pickled_session).unwrap(),
            session_id: None,
            needs_backup: false,
            backed_up_to: -1,
            sender_key: None,
            sender_data_type: None,
        };
        let session_js: JsValue = serde_wasm_bindgen::to_value(&session_dbo).unwrap();

        let key = serializer.encode_key(
            old_keys::INBOUND_GROUP_SESSIONS_V2,
            (&session.room_id, session.session_id()),
        );

        (key, session_js)
    }

    async fn measure_performance<Fut, R>(
        name: &str,
        schema: &str,
        num_records: usize,
        f: impl Fn() -> Fut,
    ) -> R
    where
        Fut: Future<Output = R>,
    {
        let window = web_sys::window().expect("should have a window in this context");
        let performance = window.performance().expect("performance should be available");
        let start = performance.now();

        let ret = f().await;

        let elapsed = performance.now() - start;
        console::log_1(
            &format!("{name} {num_records} records with {schema} schema took {elapsed:.2}ms.")
                .into(),
        );

        ret
    }

    /// Create an example InboundGroupSession of known size
    fn create_inbound_group_session(i: usize, session_key: &SessionKey) -> InboundGroupSession {
        let sender_key = Curve25519PublicKey::from_bytes([
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31,
        ]);
        let signing_key = Ed25519PublicKey::from_slice(&[
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31,
        ])
        .unwrap();
        let room_id: OwnedRoomId = format!("!a{i}:b.co").try_into().unwrap();
        let encryption_algorithm = EventEncryptionAlgorithm::MegolmV1AesSha2;
        let history_visibility = None;

        InboundGroupSession::new(
            sender_key,
            signing_key,
            &room_id,
            session_key,
            SenderData::unknown(),
            None,
            encryption_algorithm,
            history_visibility,
            false,
        )
        .unwrap()
    }

    /// Test migrating `inbound_group_sessions` data from store v5 to latest,
    /// on a store with encryption disabled.
    #[async_test]
    async fn test_v8_v10_v12_migration_unencrypted() {
        test_v8_v10_v12_migration_with_cipher("test_v8_migration_unencrypted", None).await
    }

    /// Test migrating `inbound_group_sessions` data from store v5 to store v8,
    /// on a store with encryption enabled.
    #[async_test]
    async fn test_v8_v10_v12_migration_encrypted() {
        let cipher = StoreCipher::new().unwrap();
        test_v8_v10_v12_migration_with_cipher(
            "test_v8_migration_encrypted",
            Some(Arc::new(cipher)),
        )
        .await;
    }

    /// Helper function for `test_v8_v10_v12_migration_{un,}encrypted`: test
    /// migrating `inbound_group_sessions` data from store v5 to store v12.
    async fn test_v8_v10_v12_migration_with_cipher(
        db_prefix: &str,
        store_cipher: Option<Arc<StoreCipher>>,
    ) {
        let _ = make_tracing_subscriber(None).try_init();
        let db_name = format!("{db_prefix:0}::matrix-sdk-crypto");

        // delete the db in case it was used in a previous run
        let _ = Database::delete_by_name(&db_name);

        // Given a DB with data in it as it was at v5
        let room_id = room_id!("!test:localhost");
        let (backed_up_session, not_backed_up_session) = create_sessions(&room_id);
        populate_v5_db(
            &db_name,
            store_cipher.clone(),
            &[&backed_up_session, &not_backed_up_session],
        )
        .await;

        // When I open a store based on that DB, triggering an upgrade
        let store =
            IndexeddbCryptoStore::open_with_store_cipher(&db_prefix, store_cipher).await.unwrap();

        // Then I can find the sessions using their keys and their info is correct
        let fetched_backed_up_session = store
            .get_inbound_group_session(room_id, backed_up_session.session_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched_backed_up_session.session_id(), backed_up_session.session_id());

        let fetched_not_backed_up_session = store
            .get_inbound_group_session(room_id, not_backed_up_session.session_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fetched_not_backed_up_session.session_id(), not_backed_up_session.session_id());

        // For v8: the backed_up info is preserved
        assert!(fetched_backed_up_session.backed_up());
        assert!(!fetched_not_backed_up_session.backed_up());

        // For v10: they have the backed_up_to property and it is indexed
        assert_matches_v10_schema(&db_name, &store, &fetched_backed_up_session).await;

        // For v12: they have the session_id, sender_key and sender_data_type properties
        // and they are indexed
        assert_matches_v12_schema(&db_name, &store, &fetched_backed_up_session).await;
    }

    async fn assert_matches_v10_schema(
        db_name: &str,
        store: &IndexeddbCryptoStore,
        fetched_backed_up_session: &InboundGroupSession,
    ) {
        let db = Database::open(&db_name).build().unwrap().await.unwrap();
        assert!(db.version() >= 10.0);
        let transaction = db.transaction("inbound_group_sessions3").build().unwrap();
        let raw_store = transaction.object_store("inbound_group_sessions3").unwrap();
        let key = store.serializer.encode_key(
            keys::INBOUND_GROUP_SESSIONS_V3,
            (fetched_backed_up_session.room_id(), fetched_backed_up_session.session_id()),
        );
        let idb_object: InboundGroupSessionIndexedDbObject =
            serde_wasm_bindgen::from_value(raw_store.get(&key).await.unwrap().unwrap()).unwrap();

        assert_eq!(idb_object.backed_up_to, -1);
        assert!(raw_store.index_names().find(|idx| idx == "backed_up_to").is_some());

        transaction.commit().await.unwrap();
        db.close();
    }

    async fn assert_matches_v12_schema(
        db_name: &str,
        store: &IndexeddbCryptoStore,
        session: &InboundGroupSession,
    ) {
        let db = Database::open(&db_name).build().unwrap().await.unwrap();
        assert!(db.version() >= 12.0);
        let transaction = db.transaction("inbound_group_sessions3").build().unwrap();
        let raw_store = transaction.object_store("inbound_group_sessions3").unwrap();
        let key = store
            .serializer
            .encode_key(keys::INBOUND_GROUP_SESSIONS_V3, (session.room_id(), session.session_id()));
        let idb_object: InboundGroupSessionIndexedDbObject =
            serde_wasm_bindgen::from_value(raw_store.get(&key).await.unwrap().unwrap()).unwrap();

        assert_eq!(
            idb_object.session_id,
            Some(
                store
                    .serializer
                    .encode_key_as_string(keys::INBOUND_GROUP_SESSIONS_V3, session.session_id())
            )
        );
        assert_eq!(
            idb_object.sender_key,
            Some(store.serializer.encode_key_as_string(
                keys::INBOUND_GROUP_SESSIONS_V3,
                session.sender_key().to_base64()
            ))
        );
        assert_eq!(idb_object.sender_data_type, Some(session.sender_data_type() as u8));
        assert!(
            raw_store
                .index_names()
                .find(|idx| idx == "inbound_group_session_sender_key_sender_data_type_idx")
                .is_some()
        );

        transaction.commit().await.unwrap();
        db.close();
    }

    fn create_sessions(room_id: &RoomId) -> (InboundGroupSession, InboundGroupSession) {
        let curve_key = Curve25519PublicKey::from(&Curve25519SecretKey::new());
        let ed_key = Ed25519SecretKey::new().public_key();

        let backed_up_session = InboundGroupSession::new(
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
            SenderData::legacy(),
            None,
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
            false,
        )
        .unwrap();
        backed_up_session.mark_as_backed_up();

        let not_backed_up_session = InboundGroupSession::new(
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
            SenderData::legacy(),
            None,
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
            false,
        )
        .unwrap();

        (backed_up_session, not_backed_up_session)
    }

    async fn populate_v5_db(
        db_name: &str,
        store_cipher: Option<Arc<StoreCipher>>,
        session_entries: &[&InboundGroupSession],
    ) {
        // Schema V7 migrated the inbound group sessions to a new format.
        // To test, first create a database and populate it with the *old* style of
        // entry.
        let db = create_v5_db(&db_name).await.unwrap();

        let serializer = SafeEncodeSerializer::new(store_cipher.clone());

        let txn = db
            .transaction(old_keys::INBOUND_GROUP_SESSIONS_V1)
            .with_mode(TransactionMode::Readwrite)
            .build()
            .unwrap();
        let sessions = txn.object_store(old_keys::INBOUND_GROUP_SESSIONS_V1).unwrap();
        for session in session_entries {
            let room_id = session.room_id();
            let session_id = session.session_id();
            let key =
                serializer.encode_key(old_keys::INBOUND_GROUP_SESSIONS_V1, (room_id, session_id));
            let pickle = session.pickle().await;

            // Serialize the session with the old style of serialization, since that's what
            // we used at the time.
            let serialized_session = serialize_value_as_legacy(&store_cipher, &pickle);
            sessions.put(&serialized_session).with_key(key).build().unwrap();
        }
        txn.commit().await.unwrap();

        // now close our DB, reopen it properly, and check that we can still read our
        // data.
        db.close();
    }

    /// Test migrating `backup_keys` data from store v10 to latest,
    /// on a store with encryption disabled.
    #[async_test]
    async fn test_v10_v11_migration_unencrypted() {
        test_v10_v11_migration_with_cipher("test_v10_migration_unencrypted", None).await
    }

    /// Test migrating `backup_keys` data from store v10 to latest,
    /// on a store with encryption enabled.
    #[async_test]
    async fn test_v10_v11_migration_encrypted() {
        let cipher = StoreCipher::new().unwrap();
        test_v10_v11_migration_with_cipher("test_v10_migration_encrypted", Some(Arc::new(cipher)))
            .await;
    }

    /// Helper function for `test_v10_v11_migration_{un,}encrypted`: test
    /// migrating `backup_keys` data from store v10 to store v11.
    async fn test_v10_v11_migration_with_cipher(
        db_prefix: &str,
        store_cipher: Option<Arc<StoreCipher>>,
    ) {
        let _ = make_tracing_subscriber(None).try_init();
        let db_name = format!("{db_prefix:0}::matrix-sdk-crypto");

        // delete the db in case it was used in a previous run
        let _ = Database::delete_by_name(&db_name).unwrap().await.unwrap();

        // Given a DB with data in it as it was at v5
        let db = create_v5_db(&db_name).await.unwrap();

        let txn = db
            .transaction(keys::BACKUP_KEYS)
            .with_mode(TransactionMode::Readwrite)
            .build()
            .unwrap();
        let store = txn.object_store(keys::BACKUP_KEYS).unwrap();
        store
            .put(&serialize_value_as_legacy(&store_cipher, &"1".to_owned()))
            .with_key(JsValue::from_str(old_keys::BACKUP_KEY_V1))
            .build()
            .unwrap();
        txn.commit().await.unwrap();
        db.close();

        // When I open a store based on that DB, triggering an upgrade
        let store =
            IndexeddbCryptoStore::open_with_store_cipher(&db_prefix, store_cipher).await.unwrap();

        // Then I can read the backup settings
        let backup_data = store.load_backup_keys().await.unwrap();
        assert_eq!(backup_data.backup_version, Some("1".to_owned()));
    }

    /// Test migrating `withheld_sessions` data from store v14 to latest,
    /// on a store with encryption disabled.
    #[async_test]
    async fn test_v14_v101_migration_unencrypted() {
        test_v14_v101_migration_with_cipher("test_v101_migration_unencrypted", None).await
    }

    /// Test migrating `withheld_sessions` data from store v14 to latest,
    /// on a store with encryption enabled.
    #[async_test]
    async fn test_v14_v101_migration_encrypted() {
        let cipher = StoreCipher::new().unwrap();
        test_v14_v101_migration_with_cipher(
            "test_v101_migration_encrypted",
            Some(Arc::new(cipher)),
        )
        .await;
    }

    /// Helper function for `test_v14_v101_migration_{un,}encrypted`: test
    /// migrating `withheld_sessions` data from store v14 to store v101.
    async fn test_v14_v101_migration_with_cipher(
        db_prefix: &str,
        store_cipher: Option<Arc<StoreCipher>>,
    ) {
        let serializer = SafeEncodeSerializer::new(store_cipher.clone());

        let _ = make_tracing_subscriber(None).try_init();
        let db_name = format!("{db_prefix:0}::matrix-sdk-crypto");

        // delete the db in case it was used in a previous run
        let _ = Database::delete_by_name(&db_name).unwrap().await.unwrap();

        let room_id = room_id!("!test:example.com");
        let session_id = "12345";

        // Given a DB with data in it as it was at v5
        {
            let db = create_v5_db(&db_name).await.unwrap();

            let txn = db
                .transaction(old_keys::DIRECT_WITHHELD_INFO)
                .with_mode(TransactionMode::Readwrite)
                .build()
                .unwrap();
            let store = txn.object_store(old_keys::DIRECT_WITHHELD_INFO).unwrap();

            let sender_key =
                Curve25519PublicKey::from_base64("9n7mdWKOjr9c4NTlG6zV8dbFtNK79q9vZADoh7nMUwA")
                    .unwrap();

            let withheld_entry = RoomKeyWithheldEntry {
                sender: owned_user_id!("@alice:example.com"),
                content: RoomKeyWithheldContent::new(
                    EventEncryptionAlgorithm::MegolmV1AesSha2,
                    WithheldCode::Blacklisted,
                    room_id.to_owned(),
                    session_id.to_owned(),
                    sender_key,
                    device_id!("ABC").to_owned(),
                ),
            };

            let key = serializer.encode_key(old_keys::DIRECT_WITHHELD_INFO, (room_id, session_id));
            let value = serializer.serialize_value(&withheld_entry).unwrap();
            store.add(value).with_key(key).build().unwrap();
            txn.commit().await.unwrap();
            db.close();
        }

        // When I open a store based on that DB, triggering an upgrade
        let store =
            IndexeddbCryptoStore::open_with_store_cipher(&db_prefix, store_cipher).await.unwrap();

        // Then I can read the withheld session settings
        let withheld_entry = store
            .get_withheld_info(room_id, session_id)
            .await
            .unwrap()
            .expect("Should find a withheld entry in migrated data");
        assert_eq!(withheld_entry.content.withheld_code(), WithheldCode::Blacklisted)
    }

    async fn create_v5_db(name: &str) -> std::result::Result<Database, OpenDbError> {
        v0_to_v5::schema_add(name).await?;
        Database::open(name).with_version(5u32).build()?.await
    }

    /// Opening a db that has been upgraded to MAX_SUPPORTED_SCHEMA_VERSION
    /// should be ok
    #[async_test]
    async fn test_can_open_max_supported_schema_version() {
        let _ = make_tracing_subscriber(None).try_init();

        let db_prefix = "test_can_open_max_supported_schema_version";
        // Create a database at MAX_SUPPORTED_SCHEMA_VERSION
        create_future_schema_db(db_prefix, MAX_SUPPORTED_SCHEMA_VERSION).await;

        // Now, try opening it again
        IndexeddbCryptoStore::open_with_store_cipher(&db_prefix, None).await.unwrap();
    }

    /// Opening a db that has been upgraded beyond MAX_SUPPORTED_SCHEMA_VERSION
    /// should throw an error
    #[async_test]
    async fn test_can_not_open_too_new_db() {
        let _ = make_tracing_subscriber(None).try_init();

        let db_prefix = "test_can_not_open_too_new_db";
        // Create a database at MAX_SUPPORTED_SCHEMA_VERSION+1
        create_future_schema_db(db_prefix, MAX_SUPPORTED_SCHEMA_VERSION + 1).await;

        // Now, try opening it again
        let result = IndexeddbCryptoStore::open_with_store_cipher(&db_prefix, None).await;
        assert_matches!(
            result,
            Err(IndexeddbCryptoStoreError::SchemaTooNewError {
                max_supported_version,
                current_version
            }) => {
                assert_eq!(max_supported_version, MAX_SUPPORTED_SCHEMA_VERSION);
                assert_eq!(current_version, MAX_SUPPORTED_SCHEMA_VERSION + 1);
            }
        );
    }

    // Create a database, and increase its schema version to the given version
    // number.
    async fn create_future_schema_db(db_prefix: &str, version: u32) {
        let db_name = format!("{db_prefix}::matrix-sdk-crypto");

        // delete the db in case it was used in a previous run
        let _ = Database::delete_by_name(&db_name);

        // Open, and close, the store at the regular version.
        IndexeddbCryptoStore::open_with_store_cipher(&db_prefix, None).await.unwrap();

        // Now upgrade to the given version, keeping a record of the previous version so
        // that we can double-check it.
        let old_version: Rc<Cell<Option<u32>>> = Rc::new(Cell::new(None));
        let old_version2 = old_version.clone();

        let db = Database::open(&db_name)
            .with_version(version)
            .with_on_upgrade_needed(move |evt: VersionChangeEvent, _: &Transaction<'_>| {
                old_version2.set(Some(evt.old_version() as u32));
                Ok(())
            })
            .build()
            .unwrap()
            .await
            .unwrap();

        assert_eq!(
            old_version.get(),
            Some(EXPECTED_SCHEMA_VERSION),
            "Existing store had unexpected version number"
        );
        db.close();
    }

    /// Emulate the old behaviour of [`IndexeddbSerializer::serialize_value`].
    ///
    /// We used to use an inefficient format for serializing objects in the
    /// indexeddb store. This replicates that old behaviour, for testing
    /// purposes.
    fn serialize_value_as_legacy<T: Serialize>(
        store_cipher: &Option<Arc<StoreCipher>>,
        value: &T,
    ) -> JsValue {
        if let Some(cipher) = &store_cipher {
            // Old-style serialization/encryption. First JSON-serialize into a byte array...
            let data = serde_json::to_vec(&value).unwrap();
            // ... then encrypt...
            let encrypted = cipher.encrypt_value_data(data).unwrap();
            // ... then JSON-serialize into another byte array ...
            let value = serde_json::to_vec(&encrypted).unwrap();
            // and finally, turn it into a javascript array.
            JsValue::from_serde(&value).unwrap()
        } else {
            JsValue::from_serde(&value).unwrap()
        }
    }
}
