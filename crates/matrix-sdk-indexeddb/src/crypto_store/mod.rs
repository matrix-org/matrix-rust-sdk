// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use gloo_utils::format::JsValueSerdeExt;
use hkdf::Hkdf;
use indexed_db_futures::{
    KeyRange,
    cursor::Cursor,
    database::Database,
    internals::SystemRepr,
    object_store::ObjectStore,
    prelude::*,
    transaction::{Transaction, TransactionMode},
};
use js_sys::Array;
use matrix_sdk_base::cross_process_lock::{
    CrossProcessLockGeneration, FIRST_CROSS_PROCESS_LOCK_GENERATION,
};
use matrix_sdk_crypto::{
    Account, DeviceData, GossipRequest, GossippedSecret, SecretInfo, TrackedUser, UserIdentityData,
    olm::{
        Curve25519PublicKey, InboundGroupSession, OlmMessageHash, OutboundGroupSession,
        PickledInboundGroupSession, PrivateCrossSigningIdentity, SenderDataType, Session,
        StaticAccountData,
    },
    store::{
        CryptoStore, CryptoStoreError,
        types::{
            BackupKeys, Changes, DehydratedDeviceKey, PendingChanges, RoomKeyCounts,
            RoomKeyWithheldEntry, RoomSettings, StoredRoomKeyBundleData,
        },
    },
    vodozemac::base64_encode,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, RoomId, TransactionId, UserId,
    events::secret::request::SecretName,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::Mutex;
use tracing::{debug, warn};
use wasm_bindgen::JsValue;

use crate::{
    crypto_store::migrations::open_and_upgrade_db,
    error::GenericError,
    serializer::{MaybeEncrypted, SafeEncodeSerializer, SafeEncodeSerializerError},
};

mod migrations;

mod keys {
    // stores
    pub const CORE: &str = "core";

    pub const SESSION: &str = "session";

    pub const INBOUND_GROUP_SESSIONS_V3: &str = "inbound_group_sessions3";
    pub const INBOUND_GROUP_SESSIONS_BACKUP_INDEX: &str = "backup";
    pub const INBOUND_GROUP_SESSIONS_BACKED_UP_TO_INDEX: &str = "backed_up_to";
    pub const INBOUND_GROUP_SESSIONS_SENDER_KEY_INDEX: &str =
        "inbound_group_session_sender_key_sender_data_type_idx";

    pub const OUTBOUND_GROUP_SESSIONS: &str = "outbound_group_sessions";

    pub const TRACKED_USERS: &str = "tracked_users";
    pub const OLM_HASHES: &str = "olm_hashes";

    pub const DEVICES: &str = "devices";
    pub const IDENTITIES: &str = "identities";

    pub const GOSSIP_REQUESTS: &str = "gossip_requests";
    pub const GOSSIP_REQUESTS_UNSENT_INDEX: &str = "unsent";
    pub const GOSSIP_REQUESTS_BY_INFO_INDEX: &str = "by_info";

    pub const ROOM_SETTINGS: &str = "room_settings";

    pub const SECRETS_INBOX: &str = "secrets_inbox";

    pub const WITHHELD_SESSIONS: &str = "withheld_sessions";

    pub const RECEIVED_ROOM_KEY_BUNDLES: &str = "received_room_key_bundles";

    pub const LEASE_LOCKS: &str = "lease_locks";

    pub const ROOM_KEY_BACKUPS_FULLY_DOWNLOADED: &str = "room_key_backups_fully_downloaded";

    // keys
    pub const STORE_CIPHER: &str = "store_cipher";
    pub const ACCOUNT: &str = "account";
    pub const NEXT_BATCH_TOKEN: &str = "next_batch_token";
    pub const PRIVATE_IDENTITY: &str = "private_identity";

    // backup v1
    pub const BACKUP_KEYS: &str = "backup_keys";

    /// Indexeddb key for the key backup version that [`RECOVERY_KEY_V1`]
    /// corresponds to.
    pub const BACKUP_VERSION_V1: &str = "backup_version_v1";

    /// Indexeddb key for the backup decryption key.
    ///
    /// Known, for historical reasons, as the recovery key. Not to be confused
    /// with the client-side recovery key, which is actually an AES key for use
    /// with SSSS.
    pub const RECOVERY_KEY_V1: &str = "recovery_key_v1";

    /// Indexeddb key for the dehydrated device pickle key.
    pub const DEHYDRATION_PICKLE_KEY: &str = "dehydration_pickle_key";
}

/// An implementation of [CryptoStore] that uses [IndexedDB] for persistent
/// storage.
///
/// [IndexedDB]: https://developer.mozilla.org/en-US/docs/Web/API/IndexedDB_API
pub struct IndexeddbCryptoStore {
    static_account: RwLock<Option<StaticAccountData>>,
    name: String,
    pub(crate) inner: Database,

    serializer: SafeEncodeSerializer,
    save_changes_lock: Arc<Mutex<()>>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for IndexeddbCryptoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbCryptoStore").field("name", &self.name).finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbCryptoStoreError {
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error("DomException {name} ({code}): {message}")]
    DomException {
        /// DomException code
        code: u16,
        /// Specific name of the DomException
        name: String,
        /// Message given to the DomException
        message: String,
    },
    #[error(transparent)]
    CryptoStoreError(#[from] CryptoStoreError),
    #[error(
        "The schema version of the crypto store is too new. \
         Existing version: {current_version}; max supported version: {max_supported_version}"
    )]
    SchemaTooNewError { max_supported_version: u32, current_version: u32 },
}

impl From<SafeEncodeSerializerError> for IndexeddbCryptoStoreError {
    fn from(value: SafeEncodeSerializerError) -> Self {
        match value {
            SafeEncodeSerializerError::Serialization(error) => Self::Serialization(error),
            SafeEncodeSerializerError::DomException { code, name, message } => {
                Self::DomException { code, name, message }
            }
            SafeEncodeSerializerError::CryptoStoreError(crypto_store_error) => {
                Self::CryptoStoreError(crypto_store_error)
            }
        }
    }
}

impl From<web_sys::DomException> for IndexeddbCryptoStoreError {
    fn from(frm: web_sys::DomException) -> IndexeddbCryptoStoreError {
        IndexeddbCryptoStoreError::DomException {
            name: frm.name(),
            message: frm.message(),
            code: frm.code(),
        }
    }
}

impl From<serde_wasm_bindgen::Error> for IndexeddbCryptoStoreError {
    fn from(e: serde_wasm_bindgen::Error) -> Self {
        IndexeddbCryptoStoreError::Serialization(serde::de::Error::custom(e.to_string()))
    }
}

impl From<IndexeddbCryptoStoreError> for CryptoStoreError {
    fn from(frm: IndexeddbCryptoStoreError) -> CryptoStoreError {
        match frm {
            IndexeddbCryptoStoreError::Serialization(e) => CryptoStoreError::Serialization(e),
            IndexeddbCryptoStoreError::CryptoStoreError(e) => e,
            _ => CryptoStoreError::backend(frm),
        }
    }
}

impl From<indexed_db_futures::error::DomException> for IndexeddbCryptoStoreError {
    fn from(value: indexed_db_futures::error::DomException) -> Self {
        web_sys::DomException::from(value).into()
    }
}

impl From<indexed_db_futures::error::SerialisationError> for IndexeddbCryptoStoreError {
    fn from(value: indexed_db_futures::error::SerialisationError) -> Self {
        Self::Serialization(serde::de::Error::custom(value.to_string()))
    }
}

impl From<indexed_db_futures::error::UnexpectedDataError> for IndexeddbCryptoStoreError {
    fn from(value: indexed_db_futures::error::UnexpectedDataError) -> Self {
        Self::CryptoStoreError(CryptoStoreError::backend(value))
    }
}

impl From<GenericError> for IndexeddbCryptoStoreError {
    fn from(value: GenericError) -> Self {
        Self::CryptoStoreError(value.into())
    }
}

impl From<indexed_db_futures::error::JSError> for IndexeddbCryptoStoreError {
    fn from(value: indexed_db_futures::error::JSError) -> Self {
        GenericError::from(value.to_string()).into()
    }
}

impl From<indexed_db_futures::error::Error> for IndexeddbCryptoStoreError {
    fn from(value: indexed_db_futures::error::Error) -> Self {
        use indexed_db_futures::error::Error;
        match value {
            Error::DomException(e) => e.into(),
            Error::Serialisation(e) => e.into(),
            Error::MissingData(e) => e.into(),
            Error::Unknown(e) => e.into(),
        }
    }
}

impl From<indexed_db_futures::error::OpenDbError> for IndexeddbCryptoStoreError {
    fn from(value: indexed_db_futures::error::OpenDbError) -> Self {
        use indexed_db_futures::error::OpenDbError;
        match value {
            OpenDbError::Base(error) => error.into(),
            _ => GenericError::from(value.to_string()).into(),
        }
    }
}

type Result<A, E = IndexeddbCryptoStoreError> = std::result::Result<A, E>;

/// Defines an operation to perform on the database.
enum PendingOperation {
    Put { key: JsValue, value: JsValue },
    Delete(JsValue),
}

/// A struct that represents all the operations that need to be done to the
/// database when calls to the store `save_changes` are made.
/// The idea is to do all the serialization and encryption before the
/// transaction, and then just do the actual Indexeddb operations in the
/// transaction.
struct PendingIndexeddbChanges {
    /// A map of the object store names to the operations to perform on that
    /// store.
    store_to_key_values: BTreeMap<&'static str, Vec<PendingOperation>>,
}

/// Represents the changes on a single object store.
struct PendingStoreChanges<'a> {
    operations: &'a mut Vec<PendingOperation>,
}

impl PendingStoreChanges<'_> {
    fn put(&mut self, key: JsValue, value: JsValue) {
        self.operations.push(PendingOperation::Put { key, value });
    }

    fn delete(&mut self, key: JsValue) {
        self.operations.push(PendingOperation::Delete(key));
    }
}

impl PendingIndexeddbChanges {
    fn get(&mut self, store: &'static str) -> PendingStoreChanges<'_> {
        PendingStoreChanges { operations: self.store_to_key_values.entry(store).or_default() }
    }
}

impl PendingIndexeddbChanges {
    fn new() -> Self {
        Self { store_to_key_values: BTreeMap::new() }
    }

    /// Returns the list of stores that have pending operations.
    /// Should be used as the list of store names when starting the indexeddb
    /// transaction (`transaction_on_multi_with_mode`).
    fn touched_stores(&self) -> Vec<&str> {
        self.store_to_key_values
            .iter()
            .filter_map(
                |(store, pending_operations)| {
                    if !pending_operations.is_empty() { Some(*store) } else { None }
                },
            )
            .collect()
    }

    /// Applies all the pending operations to the store.
    fn apply(self, tx: &Transaction<'_>) -> Result<()> {
        for (store, operations) in self.store_to_key_values {
            if operations.is_empty() {
                continue;
            }
            let object_store = tx.object_store(store)?;
            for op in operations {
                match op {
                    PendingOperation::Put { key, value } => {
                        object_store.put(&value).with_key(key).build()?;
                    }
                    PendingOperation::Delete(key) => {
                        object_store.delete(&key).build()?;
                    }
                }
            }
        }
        Ok(())
    }
}

impl IndexeddbCryptoStore {
    pub(crate) async fn open_with_store_cipher(
        prefix: &str,
        store_cipher: Option<Arc<StoreCipher>>,
    ) -> Result<Self> {
        let name = format!("{prefix:0}::matrix-sdk-crypto");

        let serializer = SafeEncodeSerializer::new(store_cipher);
        debug!("IndexedDbCryptoStore: opening main store {name}");
        let db = open_and_upgrade_db(&name, &serializer).await?;

        Ok(Self {
            name,
            inner: db,
            serializer,
            static_account: RwLock::new(None),
            save_changes_lock: Default::default(),
        })
    }

    /// Open a new `IndexeddbCryptoStore` with default name and no passphrase
    pub async fn open() -> Result<Self> {
        IndexeddbCryptoStore::open_with_store_cipher("crypto", None).await
    }

    /// Open an `IndexeddbCryptoStore` with given name and passphrase.
    ///
    /// If the store previously existed, the encryption cipher is initialised
    /// using the given passphrase and the details from the meta store. If the
    /// store did not previously exist, a new encryption cipher is derived
    /// from the passphrase, and the details are stored to the metastore.
    ///
    /// The store is then opened, or a new one created, using the encryption
    /// cipher.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Common prefix for the names of the two IndexedDB stores.
    /// * `passphrase` - Passphrase which is used to derive a key to encrypt the
    ///   key which is used to encrypt the store. Must be the same each time the
    ///   store is opened.
    pub async fn open_with_passphrase(prefix: &str, passphrase: &str) -> Result<Self> {
        let db = open_meta_db(prefix).await?;
        let store_cipher = load_store_cipher(&db).await?;

        let store_cipher = match store_cipher {
            Some(cipher) => {
                debug!("IndexedDbCryptoStore: decrypting store cipher");
                StoreCipher::import(passphrase, &cipher)
                    .map_err(|_| CryptoStoreError::UnpicklingError)?
            }
            None => {
                debug!("IndexedDbCryptoStore: encrypting new store cipher");
                let cipher = StoreCipher::new().map_err(CryptoStoreError::backend)?;
                #[cfg(not(test))]
                let export = cipher.export(passphrase);
                #[cfg(test)]
                let export = cipher._insecure_export_fast_for_testing(passphrase);

                let export = export.map_err(CryptoStoreError::backend)?;

                save_store_cipher(&db, &export).await?;
                cipher
            }
        };

        // Must release the database access manually as it's not done when
        // dropping it.
        db.close();

        IndexeddbCryptoStore::open_with_store_cipher(prefix, Some(store_cipher.into())).await
    }

    /// Open an `IndexeddbCryptoStore` with given name and key.
    ///
    /// If the store previously existed, the encryption cipher is initialised
    /// using the given key and the details from the meta store. If the store
    /// did not previously exist, a new encryption cipher is derived from
    /// the passphrase, and the details are stored to the metastore.
    ///
    /// The store is then opened, or a new one created, using the encryption
    /// cipher.
    ///
    /// # Arguments
    ///
    /// * `prefix` - Common prefix for the names of the two IndexedDB stores.
    /// * `key` - Key with which to encrypt the key which is used to encrypt the
    ///   store. Must be the same each time the store is opened.
    pub async fn open_with_key(prefix: &str, key: &[u8; 32]) -> Result<Self> {
        // The application might also use the provided key for something else, so to
        // avoid key reuse, we pass the provided key through an HKDF
        let mut chacha_key = zeroize::Zeroizing::new([0u8; 32]);
        const HKDF_INFO: &[u8] = b"CRYPTOSTORE_CIPHER";
        let hkdf = Hkdf::<Sha256>::new(None, key);
        hkdf.expand(HKDF_INFO, &mut *chacha_key)
            .expect("We should be able to generate a 32-byte key");

        let db = open_meta_db(prefix).await?;
        let store_cipher = load_store_cipher(&db).await?;

        let store_cipher = match store_cipher {
            Some(cipher) => {
                debug!("IndexedDbCryptoStore: decrypting store cipher");
                import_store_cipher_with_key(&chacha_key, key, &cipher, &db).await?
            }
            None => {
                debug!("IndexedDbCryptoStore: encrypting new store cipher");
                let cipher = StoreCipher::new().map_err(CryptoStoreError::backend)?;
                let export =
                    cipher.export_with_key(&chacha_key).map_err(CryptoStoreError::backend)?;
                save_store_cipher(&db, &export).await?;
                cipher
            }
        };

        // Must release the database access manually as it's not done when
        // dropping it.
        db.close();

        IndexeddbCryptoStore::open_with_store_cipher(prefix, Some(store_cipher.into())).await
    }

    /// Open a new `IndexeddbCryptoStore` with given name and no passphrase
    pub async fn open_with_name(name: &str) -> Result<Self> {
        IndexeddbCryptoStore::open_with_store_cipher(name, None).await
    }

    /// Delete the IndexedDB databases for the given name.
    #[cfg(test)]
    pub fn delete_stores(prefix: &str) -> Result<()> {
        Database::delete_by_name(&format!("{prefix:0}::matrix-sdk-crypto-meta"))?;
        Database::delete_by_name(&format!("{prefix:0}::matrix-sdk-crypto"))?;
        Ok(())
    }

    fn get_static_account(&self) -> Option<StaticAccountData> {
        self.static_account.read().unwrap().clone()
    }

    /// Transform an [`InboundGroupSession`] into a `JsValue` holding a
    /// [`InboundGroupSessionIndexedDbObject`], ready for storing.
    async fn serialize_inbound_group_session(
        &self,
        session: &InboundGroupSession,
    ) -> Result<JsValue> {
        let obj =
            InboundGroupSessionIndexedDbObject::from_session(session, &self.serializer).await?;
        Ok(serde_wasm_bindgen::to_value(&obj)?)
    }

    /// Transform a JsValue holding a [`InboundGroupSessionIndexedDbObject`]
    /// back into a [`InboundGroupSession`].
    fn deserialize_inbound_group_session(
        &self,
        stored_value: JsValue,
    ) -> Result<InboundGroupSession> {
        let idb_object: InboundGroupSessionIndexedDbObject =
            serde_wasm_bindgen::from_value(stored_value)?;
        let pickled_session: PickledInboundGroupSession =
            self.serializer.maybe_decrypt_value(idb_object.pickled_session)?;
        let session = InboundGroupSession::from_pickle(pickled_session)
            .map_err(|e| IndexeddbCryptoStoreError::CryptoStoreError(e.into()))?;

        // Although a "backed up" flag is stored inside `idb_object.pickled_session`, it
        // is not maintained when backups are reset. Overwrite the flag with the
        // needs_backup value from the IDB object.
        if idb_object.needs_backup {
            session.reset_backup_state();
        } else {
            session.mark_as_backed_up();
        }

        Ok(session)
    }

    /// Transform a [`GossipRequest`] into a `JsValue` holding a
    /// [`GossipRequestIndexedDbObject`], ready for storing.
    fn serialize_gossip_request(&self, gossip_request: &GossipRequest) -> Result<JsValue> {
        let obj = GossipRequestIndexedDbObject {
            // hash the info as a key so that it can be used in index lookups.
            info: self
                .serializer
                .encode_key_as_string(keys::GOSSIP_REQUESTS, gossip_request.info.as_key()),

            // serialize and encrypt the data about the request
            request: self.serializer.serialize_value_as_bytes(gossip_request)?,

            unsent: !gossip_request.sent_out,
        };

        Ok(serde_wasm_bindgen::to_value(&obj)?)
    }

    /// Transform a JsValue holding a [`GossipRequestIndexedDbObject`] back into
    /// a [`GossipRequest`].
    fn deserialize_gossip_request(&self, stored_request: JsValue) -> Result<GossipRequest> {
        let idb_object: GossipRequestIndexedDbObject =
            serde_wasm_bindgen::from_value(stored_request)?;
        Ok(self.serializer.deserialize_value_from_bytes(&idb_object.request)?)
    }

    /// Process all the changes and do all encryption/serialization before the
    /// actual transaction.
    ///
    /// Returns a tuple where the first item is a `PendingIndexeddbChanges`
    /// struct, and the second item is a boolean indicating whether the session
    /// cache should be cleared.
    async fn prepare_for_transaction(&self, changes: &Changes) -> Result<PendingIndexeddbChanges> {
        let mut indexeddb_changes = PendingIndexeddbChanges::new();

        let private_identity_pickle =
            if let Some(i) = &changes.private_identity { Some(i.pickle().await) } else { None };

        let decryption_key_pickle = &changes.backup_decryption_key;
        let backup_version = &changes.backup_version;
        let dehydration_pickle_key = &changes.dehydrated_device_pickle_key;

        let mut core = indexeddb_changes.get(keys::CORE);
        if let Some(next_batch) = &changes.next_batch_token {
            core.put(
                JsValue::from_str(keys::NEXT_BATCH_TOKEN),
                self.serializer.serialize_value(next_batch)?,
            );
        }

        if let Some(i) = &private_identity_pickle {
            core.put(
                JsValue::from_str(keys::PRIVATE_IDENTITY),
                self.serializer.serialize_value(i)?,
            );
        }

        if let Some(i) = &dehydration_pickle_key {
            core.put(
                JsValue::from_str(keys::DEHYDRATION_PICKLE_KEY),
                self.serializer.serialize_value(i)?,
            );
        }

        if let Some(a) = &decryption_key_pickle {
            indexeddb_changes.get(keys::BACKUP_KEYS).put(
                JsValue::from_str(keys::RECOVERY_KEY_V1),
                self.serializer.serialize_value(&a)?,
            );
        }

        if let Some(a) = &backup_version {
            indexeddb_changes.get(keys::BACKUP_KEYS).put(
                JsValue::from_str(keys::BACKUP_VERSION_V1),
                self.serializer.serialize_value(&a)?,
            );
        }

        if !changes.sessions.is_empty() {
            let mut sessions = indexeddb_changes.get(keys::SESSION);

            for session in &changes.sessions {
                let sender_key = session.sender_key().to_base64();
                let session_id = session.session_id();

                let pickle = session.pickle().await;
                let key = self.serializer.encode_key(keys::SESSION, (&sender_key, session_id));

                sessions.put(key, self.serializer.serialize_value(&pickle)?);
            }
        }

        if !changes.inbound_group_sessions.is_empty() {
            let mut sessions = indexeddb_changes.get(keys::INBOUND_GROUP_SESSIONS_V3);

            for session in &changes.inbound_group_sessions {
                let room_id = session.room_id();
                let session_id = session.session_id();
                let key = self
                    .serializer
                    .encode_key(keys::INBOUND_GROUP_SESSIONS_V3, (room_id, session_id));
                let value = self.serialize_inbound_group_session(session).await?;
                sessions.put(key, value);
            }
        }

        if !changes.outbound_group_sessions.is_empty() {
            let mut sessions = indexeddb_changes.get(keys::OUTBOUND_GROUP_SESSIONS);

            for session in &changes.outbound_group_sessions {
                let room_id = session.room_id();
                let pickle = session.pickle().await;
                sessions.put(
                    self.serializer.encode_key(keys::OUTBOUND_GROUP_SESSIONS, room_id),
                    self.serializer.serialize_value(&pickle)?,
                );
            }
        }

        let device_changes = &changes.devices;
        let identity_changes = &changes.identities;
        let olm_hashes = &changes.message_hashes;
        let key_requests = &changes.key_requests;
        let withheld_session_info = &changes.withheld_session_info;
        let room_settings_changes = &changes.room_settings;

        let mut device_store = indexeddb_changes.get(keys::DEVICES);

        for device in device_changes.new.iter().chain(&device_changes.changed) {
            let key =
                self.serializer.encode_key(keys::DEVICES, (device.user_id(), device.device_id()));
            let device = self.serializer.serialize_value(&device)?;

            device_store.put(key, device);
        }

        for device in &device_changes.deleted {
            let key =
                self.serializer.encode_key(keys::DEVICES, (device.user_id(), device.device_id()));
            device_store.delete(key);
        }

        if !identity_changes.changed.is_empty() || !identity_changes.new.is_empty() {
            let mut identities = indexeddb_changes.get(keys::IDENTITIES);
            for identity in identity_changes.changed.iter().chain(&identity_changes.new) {
                identities.put(
                    self.serializer.encode_key(keys::IDENTITIES, identity.user_id()),
                    self.serializer.serialize_value(&identity)?,
                );
            }
        }

        if !olm_hashes.is_empty() {
            let mut hashes = indexeddb_changes.get(keys::OLM_HASHES);
            for hash in olm_hashes {
                hashes.put(
                    self.serializer.encode_key(keys::OLM_HASHES, (&hash.sender_key, &hash.hash)),
                    JsValue::TRUE,
                );
            }
        }

        if !key_requests.is_empty() {
            let mut gossip_requests = indexeddb_changes.get(keys::GOSSIP_REQUESTS);

            for gossip_request in key_requests {
                let key_request_id = self
                    .serializer
                    .encode_key(keys::GOSSIP_REQUESTS, gossip_request.request_id.as_str());
                let key_request_value = self.serialize_gossip_request(gossip_request)?;
                gossip_requests.put(key_request_id, key_request_value);
            }
        }

        if !withheld_session_info.is_empty() {
            let mut withhelds = indexeddb_changes.get(keys::WITHHELD_SESSIONS);

            for (room_id, data) in withheld_session_info {
                for (session_id, event) in data {
                    let key =
                        self.serializer.encode_key(keys::WITHHELD_SESSIONS, (&room_id, session_id));
                    withhelds.put(key, self.serializer.serialize_value(&event)?);
                }
            }
        }

        if !room_settings_changes.is_empty() {
            let mut settings_store = indexeddb_changes.get(keys::ROOM_SETTINGS);

            for (room_id, settings) in room_settings_changes {
                let key = self.serializer.encode_key(keys::ROOM_SETTINGS, room_id);
                let value = self.serializer.serialize_value(&settings)?;
                settings_store.put(key, value);
            }
        }

        if !changes.secrets.is_empty() {
            let mut secret_store = indexeddb_changes.get(keys::SECRETS_INBOX);

            for secret in &changes.secrets {
                let key = self.serializer.encode_key(
                    keys::SECRETS_INBOX,
                    (secret.secret_name.as_str(), secret.event.content.request_id.as_str()),
                );
                let value = self.serializer.serialize_value(&secret)?;

                secret_store.put(key, value);
            }
        }

        if !changes.received_room_key_bundles.is_empty() {
            let mut bundle_store = indexeddb_changes.get(keys::RECEIVED_ROOM_KEY_BUNDLES);
            for bundle in &changes.received_room_key_bundles {
                let key = self.serializer.encode_key(
                    keys::RECEIVED_ROOM_KEY_BUNDLES,
                    (&bundle.bundle_data.room_id, &bundle.sender_user),
                );
                let value = self.serializer.serialize_value(&bundle)?;
                bundle_store.put(key, value);
            }
        }

        if !changes.room_key_backups_fully_downloaded.is_empty() {
            let mut room_store = indexeddb_changes.get(keys::ROOM_KEY_BACKUPS_FULLY_DOWNLOADED);
            for room_id in &changes.room_key_backups_fully_downloaded {
                room_store.put(
                    self.serializer.encode_key(keys::ROOM_KEY_BACKUPS_FULLY_DOWNLOADED, room_id),
                    JsValue::TRUE,
                );
            }
        }

        Ok(indexeddb_changes)
    }
}

// Small hack to have the following macro invocation act as the appropriate
// trait impl block on wasm, but still be compiled on non-wasm as a regular
// impl block otherwise.
//
// The trait impl doesn't compile on non-wasm due to unfulfilled trait bounds,
// this hack allows us to still have most of rust-analyzer's IDE functionality
// within the impl block without having to set it up to check things against
// the wasm target (which would disable many other parts of the codebase).
#[cfg(target_family = "wasm")]
macro_rules! impl_crypto_store {
    ( $($body:tt)* ) => {
        #[async_trait(?Send)]
        impl CryptoStore for IndexeddbCryptoStore {
            type Error = IndexeddbCryptoStoreError;

            $($body)*
        }
    };
}

#[cfg(not(target_family = "wasm"))]
macro_rules! impl_crypto_store {
    ( $($body:tt)* ) => {
        impl IndexeddbCryptoStore {
            $($body)*
        }
    };
}

impl_crypto_store! {
    async fn save_pending_changes(&self, changes: PendingChanges) -> Result<()> {
        // Serialize calls to `save_pending_changes`; there are multiple await points
        // below, and we're pickling data as we go, so we don't want to
        // invalidate data we've previously read and overwrite it in the store.
        // TODO: #2000 should make this lock go away, or change its shape.
        let _guard = self.save_changes_lock.lock().await;

        let stores: Vec<&str> = [(changes.account.is_some(), keys::CORE)]
            .iter()
            .filter_map(|(id, key)| if *id { Some(*key) } else { None })
            .collect();

        if stores.is_empty() {
            // nothing to do, quit early
            return Ok(());
        }

        let tx = self.inner.transaction(stores).with_mode(TransactionMode::Readwrite).build()?;

        let account_pickle = if let Some(account) = changes.account {
            *self.static_account.write().unwrap() = Some(account.static_data().clone());
            Some(account.pickle())
        } else {
            None
        };

        if let Some(a) = &account_pickle {
            tx.object_store(keys::CORE)?
                .put(&self.serializer.serialize_value(&a)?)
                .with_key(JsValue::from_str(keys::ACCOUNT))
                .build()?;
        }

        tx.commit().await?;

        Ok(())
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        // Serialize calls to `save_changes`; there are multiple await points below, and
        // we're pickling data as we go, so we don't want to invalidate data
        // we've previously read and overwrite it in the store.
        // TODO: #2000 should make this lock go away, or change its shape.
        let _guard = self.save_changes_lock.lock().await;

        let indexeddb_changes = self.prepare_for_transaction(&changes).await?;

        let stores = indexeddb_changes.touched_stores();

        if stores.is_empty() {
            // nothing to do, quit early
            return Ok(());
        }

        let tx = self.inner.transaction(stores).with_mode(TransactionMode::Readwrite).build()?;

        indexeddb_changes.apply(&tx)?;

        tx.commit().await?;

        Ok(())
    }

    async fn save_inbound_group_sessions(
        &self,
        sessions: Vec<InboundGroupSession>,
        backed_up_to_version: Option<&str>,
    ) -> Result<()> {
        // Sanity-check that the data in the sessions corresponds to backed_up_version
        sessions.iter().for_each(|s| {
            let backed_up = s.backed_up();
            if backed_up != backed_up_to_version.is_some() {
                warn!(
                    backed_up,
                    backed_up_to_version,
                    "Session backed-up flag does not correspond to backup version setting",
                );
            }
        });

        // Currently, this store doesn't save the backup version separately, so this
        // just delegates to save_changes.
        self.save_changes(Changes { inbound_group_sessions: sessions, ..Changes::default() }).await
    }

    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>> {
        let tx = self
            .inner
            .transaction(keys::TRACKED_USERS)
            .with_mode(TransactionMode::Readonly)
            .build()?;
        let os = tx.object_store(keys::TRACKED_USERS)?;
        let user_ids = os.get_all_keys::<JsValue>().await?;

        let mut users = Vec::new();

        for result in user_ids {
            let user_id = result?;
            let dirty: bool = !matches!(
                os.get(&user_id).await?.map(|v: JsValue| v.into_serde()),
                Some(Ok(false))
            );
            let Some(Ok(user_id)) = user_id.as_string().map(UserId::parse) else { continue };

            users.push(TrackedUser { user_id, dirty });
        }

        Ok(users)
    }

    async fn get_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        let account_info = self.get_static_account().ok_or(CryptoStoreError::AccountUnset)?;
        if let Some(value) = self
            .inner
            .transaction(keys::OUTBOUND_GROUP_SESSIONS)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::OUTBOUND_GROUP_SESSIONS)?
            .get(&self.serializer.encode_key(keys::OUTBOUND_GROUP_SESSIONS, room_id))
            .await?
        {
            Ok(Some(
                OutboundGroupSession::from_pickle(
                    account_info.device_id,
                    account_info.identity_keys,
                    self.serializer.deserialize_value(value)?,
                )
                .map_err(CryptoStoreError::from)?,
            ))
        } else {
            Ok(None)
        }
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>> {
        let jskey = self.serializer.encode_key(keys::GOSSIP_REQUESTS, request_id.as_str());
        self.inner
            .transaction(keys::GOSSIP_REQUESTS)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::GOSSIP_REQUESTS)?
            .get(jskey)
            .await?
            .map(|val| self.deserialize_gossip_request(val))
            .transpose()
    }

    async fn load_account(&self) -> Result<Option<Account>> {
        if let Some(pickle) = self
            .inner
            .transaction(keys::CORE)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(keys::ACCOUNT))
            .await?
        {
            let pickle = self.serializer.deserialize_value(pickle)?;

            let account = Account::from_pickle(pickle).map_err(CryptoStoreError::from)?;

            *self.static_account.write().unwrap() = Some(account.static_data().clone());

            Ok(Some(account))
        } else {
            Ok(None)
        }
    }

    async fn next_batch_token(&self) -> Result<Option<String>> {
        if let Some(serialized) = self
            .inner
            .transaction(keys::CORE)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(keys::NEXT_BATCH_TOKEN))
            .await?
        {
            let token = self.serializer.deserialize_value(serialized)?;
            Ok(Some(token))
        } else {
            Ok(None)
        }
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        if let Some(pickle) = self
            .inner
            .transaction(keys::CORE)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(keys::PRIVATE_IDENTITY))
            .await?
        {
            let pickle = self.serializer.deserialize_value(pickle)?;

            Ok(Some(
                PrivateCrossSigningIdentity::from_pickle(pickle)
                    .map_err(|_| CryptoStoreError::UnpicklingError)?,
            ))
        } else {
            Ok(None)
        }
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Vec<Session>>> {
        let device_keys = self.get_own_device().await?.as_device_keys().clone();

        let range = self.serializer.encode_to_range(keys::SESSION, sender_key);
        let sessions: Vec<Session> = self
            .inner
            .transaction(keys::SESSION)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::SESSION)?
            .get_all()
            .with_query(&range)
            .await?
            .filter_map(Result::ok)
            .filter_map(|f| {
                self.serializer.deserialize_value(f).ok().map(|p| {
                    Session::from_pickle(device_keys.clone(), p).map_err(|_| {
                        IndexeddbCryptoStoreError::CryptoStoreError(CryptoStoreError::AccountUnset)
                    })
                })
            })
            .collect::<Result<Vec<Session>>>()?;

        if sessions.is_empty() {
            Ok(None)
        } else {
            Ok(Some(sessions))
        }
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let key =
            self.serializer.encode_key(keys::INBOUND_GROUP_SESSIONS_V3, (room_id, session_id));
        if let Some(value) = self
            .inner
            .transaction(keys::INBOUND_GROUP_SESSIONS_V3)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::INBOUND_GROUP_SESSIONS_V3)?
            .get(&key)
            .await?
        {
            Ok(Some(self.deserialize_inbound_group_session(value)?))
        } else {
            Ok(None)
        }
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        const INBOUND_GROUP_SESSIONS_BATCH_SIZE: usize = 1000;

        let transaction = self
            .inner
            .transaction(keys::INBOUND_GROUP_SESSIONS_V3)
            .with_mode(TransactionMode::Readonly)
            .build()?;

        let object_store = transaction.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;

        fetch_from_object_store_batched(
            object_store,
            |value| self.deserialize_inbound_group_session(value),
            INBOUND_GROUP_SESSIONS_BATCH_SIZE,
        )
        .await
    }

    async fn get_inbound_group_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<InboundGroupSession>> {
        let range = self.serializer.encode_to_range(keys::INBOUND_GROUP_SESSIONS_V3, room_id);
        Ok(self
            .inner
            .transaction(keys::INBOUND_GROUP_SESSIONS_V3)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::INBOUND_GROUP_SESSIONS_V3)?
            .get_all()
            .with_query(&range)
            .await?
            .filter_map(Result::ok)
            .filter_map(|v| match self.deserialize_inbound_group_session(v) {
                Ok(session) => Some(session),
                Err(e) => {
                    warn!("Failed to deserialize inbound group session: {e}");
                    None
                }
            })
            .collect::<Vec<InboundGroupSession>>())
    }

    async fn get_inbound_group_sessions_for_device_batch(
        &self,
        sender_key: Curve25519PublicKey,
        sender_data_type: SenderDataType,
        after_session_id: Option<String>,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        let sender_key =
            self.serializer.encode_key(keys::INBOUND_GROUP_SESSIONS_V3, sender_key.to_base64());

        // The empty string is before all keys in Indexed DB - first batch starts there.
        let after_session_id = after_session_id
            .map(|s| self.serializer.encode_key(keys::INBOUND_GROUP_SESSIONS_V3, s))
            .unwrap_or("".into());

        let lower_bound: Array =
            [sender_key.clone(), (sender_data_type as u8).into(), after_session_id]
                .iter()
                .collect();
        let upper_bound: Array =
            [sender_key, ((sender_data_type as u8) + 1).into()].iter().collect();
        let key = KeyRange::Bound(lower_bound, true, upper_bound, true);

        let tx = self
            .inner
            .transaction(keys::INBOUND_GROUP_SESSIONS_V3)
            .with_mode(TransactionMode::Readonly)
            .build()?;

        let store = tx.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;
        let idx = store.index(keys::INBOUND_GROUP_SESSIONS_SENDER_KEY_INDEX)?;
        let serialized_sessions =
            idx.get_all().with_query::<Array, _>(key).with_limit(limit as u32).await?;

        // Deserialize and decrypt after the transaction is complete.
        let result = serialized_sessions
            .filter_map(Result::ok)
            .filter_map(|v| match self.deserialize_inbound_group_session(v) {
                Ok(session) => Some(session),
                Err(e) => {
                    warn!("Failed to deserialize inbound group session: {e}");
                    None
                }
            })
            .collect::<Vec<InboundGroupSession>>();

        Ok(result)
    }

    async fn inbound_group_session_counts(
        &self,
        _backup_version: Option<&str>,
    ) -> Result<RoomKeyCounts> {
        let tx = self
            .inner
            .transaction(keys::INBOUND_GROUP_SESSIONS_V3)
            .with_mode(TransactionMode::Readonly)
            .build()?;
        let store = tx.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;
        let all = store.count().await? as usize;
        let not_backed_up =
            store.index(keys::INBOUND_GROUP_SESSIONS_BACKUP_INDEX)?.count().await? as usize;
        tx.commit().await?;
        Ok(RoomKeyCounts { total: all, backed_up: all - not_backed_up })
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        _backup_version: &str,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        let tx = self
            .inner
            .transaction(keys::INBOUND_GROUP_SESSIONS_V3)
            .with_mode(TransactionMode::Readonly)
            .build()?;

        let store = tx.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;
        let idx = store.index(keys::INBOUND_GROUP_SESSIONS_BACKUP_INDEX)?;

        // XXX ideally we would use `get_all_with_key_and_limit`, but that doesn't
        // appear to be   exposed (https://github.com/Alorel/rust-indexed-db/issues/31). Instead we replicate
        //   the behaviour with a cursor.
        let Some(mut cursor) = idx.open_cursor().await? else {
            return Ok(vec![]);
        };

        let mut serialized_sessions = Vec::with_capacity(limit);
        for _ in 0..limit {
            let Some(value) = cursor.next_record().await? else {
                break;
            };
            serialized_sessions.push(value)
        }

        tx.commit().await?;

        // Deserialize and decrypt after the transaction is complete.
        let result = serialized_sessions
            .into_iter()
            .filter_map(|v| match self.deserialize_inbound_group_session(v) {
                Ok(session) => Some(session),
                Err(e) => {
                    warn!("Failed to deserialize inbound group session: {e}");
                    None
                }
            })
            .collect::<Vec<InboundGroupSession>>();

        Ok(result)
    }

    async fn mark_inbound_group_sessions_as_backed_up(
        &self,
        _backup_version: &str,
        room_and_session_ids: &[(&RoomId, &str)],
    ) -> Result<()> {
        let tx = self
            .inner
            .transaction(keys::INBOUND_GROUP_SESSIONS_V3)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        let object_store = tx.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;

        for (room_id, session_id) in room_and_session_ids {
            let key =
                self.serializer.encode_key(keys::INBOUND_GROUP_SESSIONS_V3, (room_id, session_id));
            if let Some(idb_object_js) = object_store.get(&key).await? {
                let mut idb_object: InboundGroupSessionIndexedDbObject =
                    serde_wasm_bindgen::from_value(idb_object_js)?;
                idb_object.needs_backup = false;
                object_store
                    .put(&serde_wasm_bindgen::to_value(&idb_object)?)
                    .with_key(key)
                    .build()?;
            } else {
                warn!(?key, "Could not find inbound group session to mark it as backed up.");
            }
        }

        Ok(tx.commit().await?)
    }

    async fn reset_backup_state(&self) -> Result<()> {
        let tx = self
            .inner
            .transaction(keys::INBOUND_GROUP_SESSIONS_V3)
            .with_mode(TransactionMode::Readwrite)
            .build()?;

        if let Some(mut cursor) =
            tx.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?.open_cursor().await?
        {
            while let Some(value) = cursor.next_record().await? {
                let mut idb_object: InboundGroupSessionIndexedDbObject =
                    serde_wasm_bindgen::from_value(value)?;
                if !idb_object.needs_backup {
                    idb_object.needs_backup = true;
                    // We don't bother to update the encrypted `InboundGroupSession` object stored
                    // inside `idb_object.data`, since that would require decryption and encryption.
                    // Instead, it will be patched up by `deserialize_inbound_group_session`.
                    let idb_object = serde_wasm_bindgen::to_value(&idb_object)?;
                    cursor.update(&idb_object).await?;
                }
            }
        }

        Ok(tx.commit().await?)
    }

    async fn save_tracked_users(&self, users: &[(&UserId, bool)]) -> Result<()> {
        let tx = self
            .inner
            .transaction(keys::TRACKED_USERS)
            .with_mode(TransactionMode::Readwrite)
            .build()?;
        let os = tx.object_store(keys::TRACKED_USERS)?;

        for (user, dirty) in users {
            os.put(&JsValue::from(*dirty)).with_key(JsValue::from_str(user.as_str())).build()?;
        }

        tx.commit().await?;
        Ok(())
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceData>> {
        let key = self.serializer.encode_key(keys::DEVICES, (user_id, device_id));
        self.inner
            .transaction(keys::DEVICES)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::DEVICES)?
            .get(&key)
            .await?
            .map(|i| self.serializer.deserialize_value(i).map_err(Into::into))
            .transpose()
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, DeviceData>> {
        let range = self.serializer.encode_to_range(keys::DEVICES, user_id);
        Ok(self
            .inner
            .transaction(keys::DEVICES)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::DEVICES)?
            .get_all()
            .with_query(&range)
            .await?
            .filter_map(Result::ok)
            .filter_map(|d| {
                let d: DeviceData = self.serializer.deserialize_value(d).ok()?;
                Some((d.device_id().to_owned(), d))
            })
            .collect::<HashMap<_, _>>())
    }

    async fn get_own_device(&self) -> Result<DeviceData> {
        let account_info = self.get_static_account().ok_or(CryptoStoreError::AccountUnset)?;
        Ok(self.get_device(&account_info.user_id, &account_info.device_id).await?.unwrap())
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentityData>> {
        self.inner
            .transaction(keys::IDENTITIES)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::IDENTITIES)?
            .get(&self.serializer.encode_key(keys::IDENTITIES, user_id))
            .await?
            .map(|i| self.serializer.deserialize_value(i).map_err(Into::into))
            .transpose()
    }

    async fn is_message_known(&self, hash: &OlmMessageHash) -> Result<bool> {
        Ok(self
            .inner
            .transaction(keys::OLM_HASHES)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::OLM_HASHES)?
            .get::<JsValue, _, _>(
                &self.serializer.encode_key(keys::OLM_HASHES, (&hash.sender_key, &hash.hash)),
            )
            .await?
            .is_some())
    }

    async fn get_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
    ) -> Result<Vec<GossippedSecret>> {
        let range = self.serializer.encode_to_range(keys::SECRETS_INBOX, secret_name.as_str());

        self.inner
            .transaction(keys::SECRETS_INBOX)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::SECRETS_INBOX)?
            .get_all()
            .with_query(&range)
            .await?
            .map(|result| {
                let d = result?;
                let secret = self.serializer.deserialize_value(d)?;
                Ok(secret)
            })
            .collect()
    }

    #[allow(clippy::unused_async)] // Mandated by trait on wasm.
    async fn delete_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<()> {
        let range = self.serializer.encode_to_range(keys::SECRETS_INBOX, secret_name.as_str());

        let transaction = self
            .inner
            .transaction(keys::SECRETS_INBOX)
            .with_mode(TransactionMode::Readwrite)
            .build()?;
        transaction.object_store(keys::SECRETS_INBOX)?.delete(&range).build()?;
        transaction.commit().await?;

        Ok(())
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let key = self.serializer.encode_key(keys::GOSSIP_REQUESTS, key_info.as_key());

        let val = self
            .inner
            .transaction(keys::GOSSIP_REQUESTS)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::GOSSIP_REQUESTS)?
            .index(keys::GOSSIP_REQUESTS_BY_INFO_INDEX)?
            .get(key)
            .await?;

        if let Some(val) = val {
            let deser = self.deserialize_gossip_request(val)?;
            Ok(Some(deser))
        } else {
            Ok(None)
        }
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        let results = self
            .inner
            .transaction(keys::GOSSIP_REQUESTS)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::GOSSIP_REQUESTS)?
            .index(keys::GOSSIP_REQUESTS_UNSENT_INDEX)?
            .get_all()
            .await?
            .filter_map(Result::ok)
            .filter_map(|val| self.deserialize_gossip_request(val).ok())
            .collect();

        Ok(results)
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        let jskey = self.serializer.encode_key(keys::GOSSIP_REQUESTS, request_id);
        let tx = self
            .inner
            .transaction(keys::GOSSIP_REQUESTS)
            .with_mode(TransactionMode::Readwrite)
            .build()?;
        tx.object_store(keys::GOSSIP_REQUESTS)?.delete(jskey).build()?;
        tx.commit().await.map_err(|e| e.into())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        let key = {
            let tx = self
                .inner
                .transaction(keys::BACKUP_KEYS)
                .with_mode(TransactionMode::Readonly)
                .build()?;
            let store = tx.object_store(keys::BACKUP_KEYS)?;

            let backup_version = store
                .get(&JsValue::from_str(keys::BACKUP_VERSION_V1))
                .await?
                .map(|i| self.serializer.deserialize_value(i))
                .transpose()?;

            let decryption_key = store
                .get(&JsValue::from_str(keys::RECOVERY_KEY_V1))
                .await?
                .map(|i| self.serializer.deserialize_value(i))
                .transpose()?;

            BackupKeys { backup_version, decryption_key }
        };

        Ok(key)
    }

    async fn load_dehydrated_device_pickle_key(&self) -> Result<Option<DehydratedDeviceKey>> {
        if let Some(pickle) = self
            .inner
            .transaction(keys::CORE)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(keys::DEHYDRATION_PICKLE_KEY))
            .await?
        {
            let pickle: DehydratedDeviceKey = self.serializer.deserialize_value(pickle)?;

            Ok(Some(pickle))
        } else {
            Ok(None)
        }
    }

    async fn delete_dehydrated_device_pickle_key(&self) -> Result<()> {
        self.remove_custom_value(keys::DEHYDRATION_PICKLE_KEY).await?;
        Ok(())
    }

    async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEntry>> {
        let key = self.serializer.encode_key(keys::WITHHELD_SESSIONS, (room_id, session_id));
        if let Some(pickle) = self
            .inner
            .transaction(keys::WITHHELD_SESSIONS)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::WITHHELD_SESSIONS)?
            .get(&key)
            .await?
        {
            let info = self.serializer.deserialize_value(pickle)?;
            Ok(Some(info))
        } else {
            Ok(None)
        }
    }

    async fn get_withheld_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<RoomKeyWithheldEntry>> {
        let range = self.serializer.encode_to_range(keys::WITHHELD_SESSIONS, room_id);

        self
            .inner
            .transaction(keys::WITHHELD_SESSIONS)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::WITHHELD_SESSIONS)?
            .get_all()
            .with_query(&range)
            .await?
            .map(|val| self.serializer.deserialize_value(val?).map_err(Into::into))
            .collect()
    }

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>> {
        let key = self.serializer.encode_key(keys::ROOM_SETTINGS, room_id);
        self.inner
            .transaction(keys::ROOM_SETTINGS)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::ROOM_SETTINGS)?
            .get(&key)
            .await?
            .map(|v| self.serializer.deserialize_value(v).map_err(Into::into))
            .transpose()
    }

    async fn get_received_room_key_bundle_data(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<StoredRoomKeyBundleData>> {
        let key = self.serializer.encode_key(keys::RECEIVED_ROOM_KEY_BUNDLES, (room_id, user_id));
        let result = self
            .inner
            .transaction(keys::RECEIVED_ROOM_KEY_BUNDLES)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::RECEIVED_ROOM_KEY_BUNDLES)?
            .get(&key)
            .await?
            .map(|v| self.serializer.deserialize_value(v))
            .transpose()?;

        Ok(result)
    }

    async fn has_downloaded_all_room_keys(&self, room_id: &RoomId) -> Result<bool> {
        let key = self.serializer.encode_key(keys::ROOM_KEY_BACKUPS_FULLY_DOWNLOADED, room_id);
        let result = self
            .inner
            .transaction(keys::ROOM_KEY_BACKUPS_FULLY_DOWNLOADED)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::ROOM_KEY_BACKUPS_FULLY_DOWNLOADED)?
            .get::<JsValue, _, _>(&key)
            .await?
            .is_some();

        Ok(result)
    }

    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.inner
            .transaction(keys::CORE)
            .with_mode(TransactionMode::Readonly)
            .build()?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(key))
            .await?
            .map(|v| self.serializer.deserialize_value(v).map_err(Into::into))
            .transpose()
    }

    #[allow(clippy::unused_async)] // Mandated by trait on wasm.
    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<()> {
        let transaction =
            self.inner.transaction(keys::CORE).with_mode(TransactionMode::Readwrite).build()?;
        transaction
            .object_store(keys::CORE)?
            .put(&self.serializer.serialize_value(&value)?)
            .with_key(JsValue::from_str(key))
            .build()?;
        transaction.commit().await?;
        Ok(())
    }

    #[allow(clippy::unused_async)] // Mandated by trait on wasm.
    async fn remove_custom_value(&self, key: &str) -> Result<()> {
        let transaction =
            self.inner.transaction(keys::CORE).with_mode(TransactionMode::Readwrite).build()?;
        transaction.object_store(keys::CORE)?.delete(&JsValue::from_str(key)).build()?;
        transaction.commit().await?;
        Ok(())
    }

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>> {
        // As of 2023-06-23, the code below hasn't been tested yet.
        let key = JsValue::from_str(key);
        let txn = self
            .inner
            .transaction(keys::LEASE_LOCKS)
            .with_mode(TransactionMode::Readwrite)
            .build()?;
        let object_store = txn.object_store(keys::LEASE_LOCKS)?;

        #[derive(Deserialize, Serialize)]
        struct Lease {
            holder: String,
            expiration: u64,
            generation: CrossProcessLockGeneration,
        }

        let now: u64 = MilliSecondsSinceUnixEpoch::now().get().into();
        let expiration = now + lease_duration_ms as u64;

        let lease = match object_store.get(&key).await? {
            Some(entry) => {
                let mut lease: Lease = self.serializer.deserialize_value(entry)?;

                if lease.holder == holder {
                    // We had the lease before, extend it.
                    lease.expiration = expiration;

                    Some(lease)
                } else {
                    // We didn't have it.
                    if lease.expiration < now {
                        // Steal it!
                        lease.holder = holder.to_owned();
                        lease.expiration = expiration;
                        lease.generation += 1;

                        Some(lease)
                    } else {
                        // We tried our best.
                        None
                    }
                }
            }
            None => {
                let lease = Lease {
                    holder: holder.to_owned(),
                    expiration,
                    generation: FIRST_CROSS_PROCESS_LOCK_GENERATION,
                };

                Some(lease)
            }
        };

        Ok(if let Some(lease) = lease {
            object_store.put(&self.serializer.serialize_value(&lease)?).with_key(key).build()?;

            Some(lease.generation)
        } else {
            None
        })
    }

    #[allow(clippy::unused_async)]
    async fn get_size(&self) -> Result<Option<usize>> {
        Ok(None)
    }
}

impl Drop for IndexeddbCryptoStore {
    fn drop(&mut self) {
        // Must release the database access manually as it's not done when
        // dropping it.
        self.inner.as_sys().close();
    }
}

/// Open the meta store.
///
/// The meta store contains details about the encryption of the main store.
async fn open_meta_db(prefix: &str) -> Result<Database, IndexeddbCryptoStoreError> {
    let name = format!("{prefix:0}::matrix-sdk-crypto-meta");

    debug!("IndexedDbCryptoStore: Opening meta-store {name}");
    Database::open(&name)
        .with_version(1u32)
        .with_on_upgrade_needed(|evt, tx| {
            let old_version = evt.old_version() as u32;
            if old_version < 1 {
                // migrating to version 1
                tx.db().create_object_store("matrix-sdk-crypto").build()?;
            }
            Ok(())
        })
        .await
        .map_err(Into::into)
}

/// Load the serialised store cipher from the meta store.
///
/// # Arguments:
///
/// * `meta_db`: Connection to the meta store, as returned by [`open_meta_db`].
///
/// # Returns:
///
/// The serialised `StoreCipher` object.
async fn load_store_cipher(
    meta_db: &Database,
) -> Result<Option<Vec<u8>>, IndexeddbCryptoStoreError> {
    let tx: Transaction<'_> =
        meta_db.transaction("matrix-sdk-crypto").with_mode(TransactionMode::Readonly).build()?;
    let ob = tx.object_store("matrix-sdk-crypto")?;

    let store_cipher: Option<Vec<u8>> = ob
        .get(&JsValue::from_str(keys::STORE_CIPHER))
        .await?
        .map(|k: JsValue| k.into_serde())
        .transpose()?;
    Ok(store_cipher)
}

/// Save the serialised store cipher to the meta store.
///
/// # Arguments:
///
/// * `meta_db`: Connection to the meta store, as returned by [`open_meta_db`].
/// * `store_cipher`: The serialised `StoreCipher` object.
async fn save_store_cipher(
    db: &Database,
    export: &Vec<u8>,
) -> Result<(), IndexeddbCryptoStoreError> {
    let tx: Transaction<'_> =
        db.transaction("matrix-sdk-crypto").with_mode(TransactionMode::Readwrite).build()?;
    let ob = tx.object_store("matrix-sdk-crypto")?;

    ob.put(&JsValue::from_serde(&export)?)
        .with_key(JsValue::from_str(keys::STORE_CIPHER))
        .build()?;
    tx.commit().await?;
    Ok(())
}

/// Given a serialised store cipher, try importing with the given key.
///
/// This is a helper for [`IndexeddbCryptoStore::open_with_key`].
///
/// # Arguments
///
/// * `chacha_key`: The key to use with [`StoreCipher::import_with_key`].
///   Derived from `original_key` via an HKDF.
/// * `original_key`: The key provided by the application. Used to provide a
///   migration path from an older key derivation system.
/// * `serialised_cipher`: The serialized `EncryptedStoreCipher`, retrieved from
///   the database.
/// * `db`: Connection to the database.
async fn import_store_cipher_with_key(
    chacha_key: &[u8; 32],
    original_key: &[u8],
    serialised_cipher: &[u8],
    db: &Database,
) -> Result<StoreCipher, IndexeddbCryptoStoreError> {
    let cipher = match StoreCipher::import_with_key(chacha_key, serialised_cipher) {
        Ok(cipher) => cipher,
        Err(matrix_sdk_store_encryption::Error::KdfMismatch) => {
            // Old versions of the matrix-js-sdk used to base64-encode their encryption
            // key, and pass it into [`IndexeddbCryptoStore::open_with_passphrase`]. For
            // backwards compatibility, we fall back to that if we discover we have a cipher
            // encrypted with a KDF when we expected it to be encrypted directly with a key.
            let cipher = StoreCipher::import(&base64_encode(original_key), serialised_cipher)
                .map_err(|_| CryptoStoreError::UnpicklingError)?;

            // Loading the cipher with the passphrase was successful. Let's update the
            // stored version of the cipher so that it is encrypted with a key,
            // to save doing this again.
            debug!(
                "IndexedDbCryptoStore: Migrating passphrase-encrypted store cipher to key-encryption"
            );

            let export = cipher.export_with_key(chacha_key).map_err(CryptoStoreError::backend)?;
            save_store_cipher(db, &export).await?;
            cipher
        }
        Err(_) => Err(CryptoStoreError::UnpicklingError)?,
    };
    Ok(cipher)
}

/// Fetch items from an object store in batches, transform each item using
/// the supplied function, and stuff the transformed items into a single
/// vector to return.
async fn fetch_from_object_store_batched<R, F>(
    object_store: ObjectStore<'_>,
    f: F,
    batch_size: usize,
) -> Result<Vec<R>>
where
    F: Fn(JsValue) -> Result<R>,
{
    let mut result = Vec::new();
    let mut batch_n = 0;

    // The empty string is before all keys in Indexed DB - first batch starts there.
    let mut latest_key: JsValue = "".into();

    loop {
        debug!("Fetching Indexed DB records starting from {}", batch_n * batch_size);

        // See https://github.com/Alorel/rust-indexed-db/issues/31 - we
        // would like to use `get_all_with_key_and_limit` if it ever exists
        // but for now we use a cursor and manually limit batch size.

        // Get hold of a cursor for this batch. (This should not panic in expect()
        // because we always use "", or the result of cursor.key(), both of
        // which are valid keys.)
        let after_latest_key = KeyRange::LowerBound(&latest_key, true);
        let cursor = object_store.open_cursor().with_query(&after_latest_key).await?;

        // Fetch batch_size records into result
        let next_key = fetch_batch(cursor, batch_size, &f, &mut result).await?;
        if let Some(next_key) = next_key {
            latest_key = next_key;
        } else {
            break;
        }

        batch_n += 1;
    }

    Ok(result)
}

/// Fetch batch_size records from the supplied cursor,
/// and return the last key we processed, or None if
/// we reached the end of the cursor.
async fn fetch_batch<R, F, Q>(
    cursor: Option<Cursor<'_, Q>>,
    batch_size: usize,
    f: &F,
    result: &mut Vec<R>,
) -> Result<Option<JsValue>>
where
    F: Fn(JsValue) -> Result<R>,
    Q: QuerySource,
{
    let Some(mut cursor) = cursor else {
        // Cursor was None - there are no more records
        return Ok(None);
    };

    let mut latest_key = None;

    for _ in 0..batch_size {
        let Some(value) = cursor.next_record().await? else {
            return Ok(None);
        };

        // Process the record
        let processed = f(value);
        if let Ok(processed) = processed {
            result.push(processed);
        }
        // else processing failed: don't return this record at all

        // Remember that we have processed this record, so if we hit
        // the end of the batch, the next batch can start after this one
        if let Some(key) = cursor.key()? {
            latest_key = Some(key);
        }
    }

    // We finished the batch but there are more records -
    // return the key of the last one we processed
    Ok(latest_key)
}

/// The objects we store in the gossip_requests indexeddb object store
#[derive(Debug, Serialize, Deserialize)]
struct GossipRequestIndexedDbObject {
    /// Encrypted hash of the [`SecretInfo`] structure.
    info: String,

    /// Encrypted serialised representation of the [`GossipRequest`] as a whole.
    request: Vec<u8>,

    /// Whether the request has yet to be sent out.
    ///
    /// Since we only need to be able to find requests where this is `true`, we
    /// skip serialization in cases where it is `false`. That has the effect
    /// of omitting it from the indexeddb index.
    ///
    /// We also use a custom serializer because bools can't be used as keys in
    /// indexeddb.
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        with = "crate::serializer::foreign::bool"
    )]
    unsent: bool,
}

/// The objects we store in the inbound_group_sessions3 indexeddb object store
#[derive(Serialize, Deserialize)]
struct InboundGroupSessionIndexedDbObject {
    /// Possibly encrypted
    /// [`matrix_sdk_crypto::olm::group_sessions::PickledInboundGroupSession`]
    pickled_session: MaybeEncrypted,

    /// The (hashed) session ID of this session. This is somewhat redundant, but
    /// we have to pull it out to its own object so that we can do batched
    /// queries such as
    /// [`IndexeddbStore::get_inbound_group_sessions_for_device_batch`].
    ///
    /// Added in database schema v12, and lazily populated, so it is only
    /// present for sessions received or modified since DB schema v12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_id: Option<String>,

    /// Whether the session data has yet to be backed up.
    ///
    /// Since we only need to be able to find entries where this is `true`, we
    /// skip serialization in cases where it is `false`. That has the effect
    /// of omitting it from the indexeddb index.
    ///
    /// We also use a custom serializer because bools can't be used as keys in
    /// indexeddb.
    #[serde(
        default,
        skip_serializing_if = "std::ops::Not::not",
        with = "crate::serializer::foreign::bool"
    )]
    needs_backup: bool,

    /// Unused: for future compatibility. In future, will contain the order
    /// number (not the ID!) of the backup for which this key has been
    /// backed up. This will replace `needs_backup`, fixing the performance
    /// problem identified in
    /// https://github.com/element-hq/element-web/issues/26892
    /// because we won't need to update all records when we spot a new backup
    /// version.
    /// In this version of the code, this is always set to -1, meaning:
    /// "refer to the `needs_backup` property". See:
    /// https://github.com/element-hq/element-web/issues/26892#issuecomment-1906336076
    backed_up_to: i32,

    /// The (hashed) curve25519 key of the device that sent us this room key,
    /// base64-encoded.
    ///
    /// Added in database schema v12, and lazily populated, so it is only
    /// present for sessions received or modified since DB schema v12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sender_key: Option<String>,

    /// The type of the [`SenderData`] within this session, converted to a u8
    /// from [`SenderDataType`].
    ///
    /// Added in database schema v12, and lazily populated, so it is only
    /// present for sessions received or modified since DB schema v12.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sender_data_type: Option<u8>,
}

impl InboundGroupSessionIndexedDbObject {
    /// Build an [`InboundGroupSessionIndexedDbObject`] wrapping the given
    /// session.
    pub async fn from_session(
        session: &InboundGroupSession,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self, CryptoStoreError> {
        let session_id =
            serializer.encode_key_as_string(keys::INBOUND_GROUP_SESSIONS_V3, session.session_id());

        let sender_key = serializer.encode_key_as_string(
            keys::INBOUND_GROUP_SESSIONS_V3,
            session.sender_key().to_base64(),
        );

        Ok(InboundGroupSessionIndexedDbObject {
            pickled_session: serializer.maybe_encrypt_value(session.pickle().await)?,
            session_id: Some(session_id),
            needs_backup: !session.backed_up(),
            backed_up_to: -1,
            sender_key: Some(sender_key),
            sender_data_type: Some(session.sender_data_type() as u8),
        })
    }
}

#[cfg(test)]
mod unit_tests {
    use matrix_sdk_crypto::{
        olm::{Curve25519PublicKey, InboundGroupSession, SenderData, SessionKey},
        types::EventEncryptionAlgorithm,
        vodozemac::Ed25519Keypair,
    };
    use matrix_sdk_store_encryption::EncryptedValueBase64;
    use matrix_sdk_test::async_test;
    use ruma::{device_id, room_id, user_id};

    use super::InboundGroupSessionIndexedDbObject;
    use crate::serializer::{MaybeEncrypted, SafeEncodeSerializer};

    #[test]
    fn needs_backup_is_serialized_as_a_u8_in_json() {
        let session_needs_backup = backup_test_session(true);

        // Testing the exact JSON here is theoretically flaky in the face of
        // serialization changes in serde_json but it seems unlikely, and it's
        // simple enough to fix if we need to.
        assert!(
            serde_json::to_string(&session_needs_backup).unwrap().contains(r#""needs_backup":1"#),
        );
    }

    #[test]
    fn doesnt_need_backup_is_serialized_with_missing_field_in_json() {
        let session_backed_up = backup_test_session(false);

        assert!(
            !serde_json::to_string(&session_backed_up).unwrap().contains("needs_backup"),
            "The needs_backup field should be missing!"
        );
    }

    pub fn backup_test_session(needs_backup: bool) -> InboundGroupSessionIndexedDbObject {
        InboundGroupSessionIndexedDbObject {
            pickled_session: MaybeEncrypted::Encrypted(EncryptedValueBase64::new(1, "", "")),
            session_id: None,
            needs_backup,
            backed_up_to: -1,
            sender_key: None,
            sender_data_type: None,
        }
    }

    #[async_test]
    async fn test_sender_key_and_sender_data_type_are_serialized_in_json() {
        let sender_key = Curve25519PublicKey::from_bytes([0; 32]);

        let sender_data = SenderData::sender_verified(
            user_id!("@test:user"),
            device_id!("ABC"),
            Ed25519Keypair::new().public_key(),
        );

        let db_object = sender_data_test_session(sender_key, sender_data).await;
        let serialized = serde_json::to_string(&db_object).unwrap();

        assert!(
            serialized.contains(r#""sender_key":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA""#)
        );
        assert!(serialized.contains(r#""sender_data_type":5"#));
    }

    pub async fn sender_data_test_session(
        sender_key: Curve25519PublicKey,
        sender_data: SenderData,
    ) -> InboundGroupSessionIndexedDbObject {
        let session = InboundGroupSession::new(
            sender_key,
            Ed25519Keypair::new().public_key(),
            room_id!("!test:localhost"),
            // Arbitrary session data
            &SessionKey::from_base64(
                "AgAAAABTyn3CR8mzAxhsHH88td5DrRqfipJCnNbZeMrfzhON6O1Cyr9ewx/sDFLO6\
                 +NvyW92yGvMub7nuAEQb+SgnZLm7nwvuVvJgSZKpoJMVliwg8iY9TXKFT286oBtT2\
                 /8idy6TcpKax4foSHdMYlZXu5zOsGDdd9eYnYHpUEyDT0utuiaakZM3XBMNLEVDj9\
                 Ps929j1FGgne1bDeFVoty2UAOQK8s/0JJigbKSu6wQ/SzaCYpE/LD4Egk2Nxs1JE2\
                 33ii9J8RGPYOp7QWl0kTEc8mAlqZL7mKppo9AwgtmYweAg",
            )
            .unwrap(),
            sender_data,
            None,
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            None,
            false,
        )
        .unwrap();

        InboundGroupSessionIndexedDbObject::from_session(&session, &SafeEncodeSerializer::new(None))
            .await
            .unwrap()
    }
}

#[cfg(all(test, target_family = "wasm"))]
mod wasm_unit_tests {
    use std::collections::BTreeMap;

    use matrix_sdk_crypto::{
        olm::{Curve25519PublicKey, SenderData},
        types::{DeviceKeys, Signatures},
    };
    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id};
    use wasm_bindgen::JsValue;

    use crate::crypto_store::unit_tests::sender_data_test_session;

    fn assert_field_equals(js_value: &JsValue, field: &str, expected: u32) {
        assert_eq!(
            js_sys::Reflect::get(&js_value, &field.into()).unwrap(),
            JsValue::from_f64(expected.into())
        );
    }

    #[async_test]
    fn test_needs_backup_is_serialized_as_a_u8_in_js() {
        let session_needs_backup = super::unit_tests::backup_test_session(true);

        let js_value = serde_wasm_bindgen::to_value(&session_needs_backup).unwrap();

        assert!(js_value.is_object());
        assert_field_equals(&js_value, "needs_backup", 1);
    }

    #[async_test]
    fn test_doesnt_need_backup_is_serialized_with_missing_field_in_js() {
        let session_backed_up = super::unit_tests::backup_test_session(false);

        let js_value = serde_wasm_bindgen::to_value(&session_backed_up).unwrap();

        assert!(!js_sys::Reflect::has(&js_value, &"needs_backup".into()).unwrap());
    }

    #[async_test]
    async fn test_sender_key_and_device_type_are_serialized_in_js() {
        let sender_key = Curve25519PublicKey::from_bytes([0; 32]);

        let sender_data = SenderData::device_info(DeviceKeys::new(
            user_id!("@test:user").to_owned(),
            device_id!("ABC").to_owned(),
            vec![],
            BTreeMap::new(),
            Signatures::new(),
        ));
        let db_object = sender_data_test_session(sender_key, sender_data).await;

        let js_value = serde_wasm_bindgen::to_value(&db_object).unwrap();

        assert!(js_value.is_object());
        assert_field_equals(&js_value, "sender_data_type", 2);
        assert_eq!(
            js_sys::Reflect::get(&js_value, &"sender_key".into()).unwrap(),
            JsValue::from_str("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
        );
    }
}

#[cfg(all(test, target_family = "wasm"))]
mod tests {
    use matrix_sdk_crypto::cryptostore_integration_tests;

    use super::IndexeddbCryptoStore;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    async fn get_store(
        name: &str,
        passphrase: Option<&str>,
        clear_data: bool,
    ) -> IndexeddbCryptoStore {
        if clear_data {
            IndexeddbCryptoStore::delete_stores(name).unwrap();
        }
        match passphrase {
            Some(pass) => IndexeddbCryptoStore::open_with_passphrase(name, pass)
                .await
                .expect("Can't create a passphrase protected store"),
            None => IndexeddbCryptoStore::open_with_name(name)
                .await
                .expect("Can't create store without passphrase"),
        }
    }

    cryptostore_integration_tests!();
}

#[cfg(all(test, target_family = "wasm"))]
mod encrypted_tests {
    use matrix_sdk_crypto::{
        cryptostore_integration_tests,
        olm::Account,
        store::{CryptoStore, types::PendingChanges},
        vodozemac::base64_encode,
    };
    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id};

    use super::IndexeddbCryptoStore;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    async fn get_store(
        name: &str,
        passphrase: Option<&str>,
        clear_data: bool,
    ) -> IndexeddbCryptoStore {
        if clear_data {
            IndexeddbCryptoStore::delete_stores(name).unwrap();
        }

        let pass = passphrase.unwrap_or(name);
        IndexeddbCryptoStore::open_with_passphrase(&name, pass)
            .await
            .expect("Can't create a passphrase protected store")
    }
    cryptostore_integration_tests!();

    /// Test that we can migrate a store created with a passphrase, to being
    /// encrypted with a key instead.
    #[async_test]
    async fn test_migrate_passphrase_to_key() {
        let store_name = "test_migrate_passphrase_to_key";
        let passdata: [u8; 32] = rand::random();
        let b64_passdata = base64_encode(passdata);

        // Initialise the store with some account data
        IndexeddbCryptoStore::delete_stores(store_name).unwrap();
        let store = IndexeddbCryptoStore::open_with_passphrase(&store_name, &b64_passdata)
            .await
            .expect("Can't create a passphrase-protected store");

        store
            .save_pending_changes(PendingChanges {
                account: Some(Account::with_device_id(
                    user_id!("@alice:example.org"),
                    device_id!("ALICEDEVICE"),
                )),
            })
            .await
            .expect("Can't save account");

        // Now reopen the store, passing the key directly rather than as a b64 string.
        let store = IndexeddbCryptoStore::open_with_key(&store_name, &passdata)
            .await
            .expect("Can't create a key-protected store");
        let loaded_account =
            store.load_account().await.expect("Can't load account").expect("Account was not saved");
        assert_eq!(loaded_account.user_id, user_id!("@alice:example.org"));
    }
}
