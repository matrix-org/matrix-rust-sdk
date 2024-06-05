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
use indexed_db_futures::prelude::*;
use matrix_sdk_crypto::{
    olm::{
        InboundGroupSession, OlmMessageHash, OutboundGroupSession, PickledInboundGroupSession,
        PrivateCrossSigningIdentity, Session, StaticAccountData,
    },
    store::{
        caches::SessionStore, BackupKeys, Changes, CryptoStore, CryptoStoreError, PendingChanges,
        RoomKeyCounts, RoomSettings,
    },
    types::events::room_key_withheld::RoomKeyWithheldEvent,
    vodozemac::base64_encode,
    Account, GossipRequest, GossippedSecret, ReadOnlyDevice, ReadOnlyUserIdentities, SecretInfo,
    TrackedUser,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{
    events::secret::request::SecretName, DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId,
    RoomId, TransactionId, UserId,
};
use sha2::Sha256;
use tokio::sync::Mutex;
use tracing::{debug, warn};
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;

use self::indexeddb_serializer::MaybeEncrypted;
use crate::crypto_store::{
    indexeddb_serializer::IndexeddbSerializer, migrations::open_and_upgrade_db,
};

mod indexeddb_serializer;
mod migrations;

mod keys {
    // stores
    pub const CORE: &str = "core";

    pub const SESSION: &str = "session";

    pub const INBOUND_GROUP_SESSIONS_V3: &str = "inbound_group_sessions3";
    pub const INBOUND_GROUP_SESSIONS_BACKUP_INDEX: &str = "backup";
    pub const INBOUND_GROUP_SESSIONS_BACKED_UP_TO_INDEX: &str = "backed_up_to";

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

    pub const DIRECT_WITHHELD_INFO: &str = "direct_withheld_info";

    // keys
    pub const STORE_CIPHER: &str = "store_cipher";
    pub const ACCOUNT: &str = "account";
    pub const NEXT_BATCH_TOKEN: &str = "next_batch_token";
    pub const PRIVATE_IDENTITY: &str = "private_identity";

    // backup v1
    pub const BACKUP_KEYS: &str = "backup_keys";
    pub const BACKUP_KEY_V1: &str = "backup_key_v1";
    /// Indexeddb key for the backup decryption key.
    ///
    /// Known, for historical reasons, as the recovery key. Not to be confused
    /// with the client-side recovery key, which is actually an AES key for use
    /// with SSSS.
    pub const RECOVERY_KEY_V1: &str = "recovery_key_v1";
}

/// An implementation of [CryptoStore] that uses [IndexedDB] for persistent
/// storage.
///
/// [IndexedDB]: https://developer.mozilla.org/en-US/docs/Web/API/IndexedDB_API
pub struct IndexeddbCryptoStore {
    static_account: RwLock<Option<StaticAccountData>>,
    name: String,
    pub(crate) inner: IdbDatabase,

    serializer: IndexeddbSerializer,
    session_cache: SessionStore,
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

impl<'a> PendingStoreChanges<'a> {
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
                    if !pending_operations.is_empty() {
                        Some(*store)
                    } else {
                        None
                    }
                },
            )
            .collect()
    }

    /// Applies all the pending operations to the store.
    fn apply(self, tx: &IdbTransaction<'_>) -> Result<()> {
        for (store, operations) in self.store_to_key_values {
            if operations.is_empty() {
                continue;
            }
            let object_store = tx.object_store(store)?;
            for op in operations {
                match op {
                    PendingOperation::Put { key, value } => {
                        object_store.put_key_val(&key, &value)?;
                    }
                    PendingOperation::Delete(key) => {
                        object_store.delete(&key)?;
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

        let serializer = IndexeddbSerializer::new(store_cipher);
        debug!("IndexedDbCryptoStore: opening main store {name}");
        let db = open_and_upgrade_db(&name, &serializer).await?;
        let session_cache = SessionStore::new();

        Ok(Self {
            name,
            session_cache,
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

    fn get_static_account(&self) -> Option<StaticAccountData> {
        self.static_account.read().unwrap().clone()
    }

    /// Transform an [`InboundGroupSession`] into a `JsValue` holding a
    /// [`InboundGroupSessionIndexedDbObject`], ready for storing.
    async fn serialize_inbound_group_session(
        &self,
        session: &InboundGroupSession,
    ) -> Result<JsValue> {
        let obj = InboundGroupSessionIndexedDbObject::new(
            self.serializer.maybe_encrypt_value(session.pickle().await)?,
            !session.backed_up(),
        );
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
    async fn prepare_for_transaction(&self, changes: &Changes) -> Result<PendingIndexeddbChanges> {
        let mut indexeddb_changes = PendingIndexeddbChanges::new();

        let private_identity_pickle =
            if let Some(i) = &changes.private_identity { Some(i.pickle().await) } else { None };

        let decryption_key_pickle = &changes.backup_decryption_key;
        let backup_version = &changes.backup_version;

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

        if let Some(a) = &decryption_key_pickle {
            indexeddb_changes.get(keys::BACKUP_KEYS).put(
                JsValue::from_str(keys::RECOVERY_KEY_V1),
                self.serializer.serialize_value(&a)?,
            );
        }

        if let Some(a) = &backup_version {
            indexeddb_changes
                .get(keys::BACKUP_KEYS)
                .put(JsValue::from_str(keys::BACKUP_KEY_V1), self.serializer.serialize_value(&a)?);
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
            let mut withhelds = indexeddb_changes.get(keys::DIRECT_WITHHELD_INFO);

            for (room_id, data) in withheld_session_info {
                for (session_id, event) in data {
                    let key = self
                        .serializer
                        .encode_key(keys::DIRECT_WITHHELD_INFO, (session_id, &room_id));
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
#[cfg(target_arch = "wasm32")]
macro_rules! impl_crypto_store {
    ( $($body:tt)* ) => {
        #[async_trait(?Send)]
        impl CryptoStore for IndexeddbCryptoStore {
            type Error = IndexeddbCryptoStoreError;

            $($body)*
        }
    };
}

#[cfg(not(target_arch = "wasm32"))]
macro_rules! impl_crypto_store {
    ( $($body:tt)* ) => {
        impl IndexeddbCryptoStore {
            $($body)*
        }
    };
}

impl_crypto_store! {
    async fn save_pending_changes(&self, changes: PendingChanges) -> Result<()> {
        // Serialize calls to `save_pending_changes`; there are multiple await points below, and we're
        // pickling data as we go, so we don't want to invalidate data we've previously read and
        // overwrite it in the store.
        // TODO: #2000 should make this lock go away, or change its shape.
        let _guard = self.save_changes_lock.lock().await;

        let stores: Vec<&str> = [
            (changes.account.is_some() , keys::CORE),
        ]
        .iter()
        .filter_map(|(id, key)| if *id { Some(*key) } else { None })
        .collect();

        if stores.is_empty() {
            // nothing to do, quit early
            return Ok(());
        }

        let tx =
            self.inner.transaction_on_multi_with_mode(&stores, IdbTransactionMode::Readwrite)?;

        let account_pickle = if let Some(account) = changes.account {
            *self.static_account.write().unwrap() = Some(account.static_data().clone());
            Some(account.pickle())
        } else {
            None
        };

        if let Some(a) = &account_pickle {
            tx.object_store(keys::CORE)?
                .put_key_val(&JsValue::from_str(keys::ACCOUNT), &self.serializer.serialize_value(&a)?)?;
        }

        tx.await.into_result()?;

        Ok(())
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        // Serialize calls to `save_changes`; there are multiple await points below, and we're
        // pickling data as we go, so we don't want to invalidate data we've previously read and
        // overwrite it in the store.
        // TODO: #2000 should make this lock go away, or change its shape.
        let _guard = self.save_changes_lock.lock().await;

        let indexeddb_changes = self.prepare_for_transaction(&changes).await?;

        let stores = indexeddb_changes.touched_stores();

        if stores.is_empty() {
            // nothing to do, quit early
            return Ok(());
        }

        let tx =
            self.inner.transaction_on_multi_with_mode(&stores, IdbTransactionMode::Readwrite)?;

        indexeddb_changes.apply(&tx)?;

        tx.await.into_result()?;

        // all good, let's update our caches:indexeddb
        for session in changes.sessions {
            self.session_cache.add(session).await;
        }

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
                    backed_up, backed_up_to_version,
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
            .transaction_on_one_with_mode(keys::TRACKED_USERS, IdbTransactionMode::Readonly)?;
        let os = tx.object_store(keys::TRACKED_USERS)?;
        let user_ids = os.get_all_keys()?.await?;

        let mut users = Vec::new();

        for user_id in user_ids.iter() {
            let dirty: bool =
                !matches!(os.get(&user_id)?.await?.map(|v| v.into_serde()), Some(Ok(false)));
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
            .transaction_on_one_with_mode(
                keys::OUTBOUND_GROUP_SESSIONS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::OUTBOUND_GROUP_SESSIONS)?
            .get(&self.serializer.encode_key(keys::OUTBOUND_GROUP_SESSIONS, room_id))?
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
        self
            .inner
            .transaction_on_one_with_mode(keys::GOSSIP_REQUESTS, IdbTransactionMode::Readonly)?
            .object_store(keys::GOSSIP_REQUESTS)?
            .get_owned(jskey)?
            .await?
            .map(|val| self.deserialize_gossip_request(val))
            .transpose()
    }

    async fn load_account(&self) -> Result<Option<Account>> {
        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readonly)?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(keys::ACCOUNT))?
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
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readonly)?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(keys::NEXT_BATCH_TOKEN))?
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
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readonly)?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(keys::PRIVATE_IDENTITY))?
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

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let account_info = self.get_static_account().ok_or(CryptoStoreError::AccountUnset)?;

        if self.session_cache.get(sender_key).is_none() {
            let range = self.serializer.encode_to_range(keys::SESSION, sender_key)?;
            let sessions: Vec<Session> = self
                .inner
                .transaction_on_one_with_mode(keys::SESSION, IdbTransactionMode::Readonly)?
                .object_store(keys::SESSION)?
                .get_all_with_key(&range)?
                .await?
                .iter()
                .filter_map(|f| self.serializer.deserialize_value(f).ok().map(|p| {
                    Session::from_pickle(
                        account_info.user_id.clone(),
                        account_info.device_id.clone(),
                        account_info.identity_keys.clone(),
                        p,
                    )
                }))
                .collect::<Vec<Session>>();

            self.session_cache.set_for_sender(sender_key, sessions);
        }

        Ok(self.session_cache.get(sender_key))
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let key = self.serializer.encode_key(keys::INBOUND_GROUP_SESSIONS_V3, (room_id, session_id));
        if let Some(value) = self
            .inner
            .transaction_on_one_with_mode(
                keys::INBOUND_GROUP_SESSIONS_V3,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::INBOUND_GROUP_SESSIONS_V3)?
            .get(&key)?
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
            .transaction_on_one_with_mode(
                keys::INBOUND_GROUP_SESSIONS_V3,
                IdbTransactionMode::Readonly,
            )?;

        let object_store = transaction.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;

        fetch_from_object_store_batched(
            object_store,
            |value| self.deserialize_inbound_group_session(value),
            INBOUND_GROUP_SESSIONS_BATCH_SIZE
        ).await
    }

    async fn inbound_group_session_counts(&self, _backup_version: Option<&str>) -> Result<RoomKeyCounts> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(
                keys::INBOUND_GROUP_SESSIONS_V3,
                IdbTransactionMode::Readonly,
            )?;
        let store = tx.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;
        let all = store.count()?.await? as usize;
        let not_backed_up = store.index(keys::INBOUND_GROUP_SESSIONS_BACKUP_INDEX)?.count()?.await? as usize;
        tx.await.into_result()?;
        Ok(RoomKeyCounts { total: all, backed_up: all - not_backed_up })
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        _backup_version: &str,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(
                keys::INBOUND_GROUP_SESSIONS_V3,
                IdbTransactionMode::Readonly,
            )?;


        let store = tx.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;
        let idx = store.index(keys::INBOUND_GROUP_SESSIONS_BACKUP_INDEX)?;

        // XXX ideally we would use `get_all_with_key_and_limit`, but that doesn't appear to be
        //   exposed (https://github.com/Alorel/rust-indexed-db/issues/31). Instead we replicate
        //   the behaviour with a cursor.
        let Some(cursor) = idx.open_cursor()?.await? else {
            return Ok(vec![]);
        };

        let mut serialized_sessions = Vec::with_capacity(limit);
        for _ in 0..limit {
            serialized_sessions.push(cursor.value());
            if !cursor.continue_cursor()?.await? {
                break;
            }
        }

        tx.await.into_result()?;

        // Deserialize and decrypt after the transaction is complete.
        let result = serialized_sessions.into_iter()
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

    async fn mark_inbound_group_sessions_as_backed_up(&self,
        _backup_version: &str,
        room_and_session_ids: &[(&RoomId, &str)]
    ) -> Result<()> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(
                keys::INBOUND_GROUP_SESSIONS_V3,
                IdbTransactionMode::Readwrite,
            )?;

        let object_store = tx.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?;

        for (room_id, session_id) in room_and_session_ids {
            let key = self.serializer.encode_key(keys::INBOUND_GROUP_SESSIONS_V3, (room_id, session_id));
            if let Some(idb_object_js) = object_store.get(&key)?.await? {
                let mut idb_object: InboundGroupSessionIndexedDbObject = serde_wasm_bindgen::from_value(idb_object_js)?;
                idb_object.needs_backup = false;
                object_store.put_key_val(&key, &serde_wasm_bindgen::to_value(&idb_object)?)?;
            } else {
                warn!("Could not find inbound group session to mark it as backed up. key={:?}", key);
            }
        }

        Ok(tx.await.into_result()?)
    }

    async fn reset_backup_state(&self) -> Result<()> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(
                keys::INBOUND_GROUP_SESSIONS_V3,
                IdbTransactionMode::Readwrite,
            )?;

        if let Some(cursor) = tx.object_store(keys::INBOUND_GROUP_SESSIONS_V3)?.open_cursor()?.await? {
            loop {
                let mut idb_object: InboundGroupSessionIndexedDbObject = serde_wasm_bindgen::from_value(cursor.value())?;
                if !idb_object.needs_backup {
                    idb_object.needs_backup = true;
                    // We don't bother to update the encrypted `InboundGroupSession` object stored
                    // inside `idb_object.data`, since that would require decryption and encryption.
                    // Instead, it will be patched up by `deserialize_inbound_group_session`.
                    let idb_object = serde_wasm_bindgen::to_value(&idb_object)?;
                    cursor.update(&idb_object)?.await?;
                }

                if !cursor.continue_cursor()?.await? {
                    break;
                }
            }
        }

        Ok(tx.await.into_result()?)
    }

    async fn save_tracked_users(&self, users: &[(&UserId, bool)]) -> Result<()> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(keys::TRACKED_USERS, IdbTransactionMode::Readwrite)?;
        let os = tx.object_store(keys::TRACKED_USERS)?;

        for (user, dirty) in users {
            os.put_key_val(&JsValue::from_str(user.as_str()), &JsValue::from(*dirty))?;
        }

        tx.await.into_result()?;
        Ok(())
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let key = self.serializer.encode_key(keys::DEVICES, (user_id, device_id));
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::DEVICES, IdbTransactionMode::Readonly)?
            .object_store(keys::DEVICES)?
            .get(&key)?
            .await?
            .map(|i| self.serializer.deserialize_value(i))
            .transpose()?)
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        let range = self.serializer.encode_to_range(keys::DEVICES, user_id)?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::DEVICES, IdbTransactionMode::Readonly)?
            .object_store(keys::DEVICES)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|d| {
                let d: ReadOnlyDevice = self.serializer.deserialize_value(d).ok()?;
                Some((d.device_id().to_owned(), d))
            })
            .collect::<HashMap<_, _>>())
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::IDENTITIES, IdbTransactionMode::Readonly)?
            .object_store(keys::IDENTITIES)?
            .get(&self.serializer.encode_key(keys::IDENTITIES, user_id))?
            .await?
            .map(|i| self.serializer.deserialize_value(i))
            .transpose()?)
    }

    async fn is_message_known(&self, hash: &OlmMessageHash) -> Result<bool> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::OLM_HASHES, IdbTransactionMode::Readonly)?
            .object_store(keys::OLM_HASHES)?
            .get(&self.serializer.encode_key(keys::OLM_HASHES, (&hash.sender_key, &hash.hash)))?
            .await?
            .is_some())
    }

    async fn get_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
        ) -> Result<Vec<GossippedSecret>> {
        let range = self.serializer.encode_to_range(keys::SECRETS_INBOX, secret_name.as_str())?;

        self
            .inner
            .transaction_on_one_with_mode(keys::SECRETS_INBOX, IdbTransactionMode::Readonly)?
            .object_store(keys::SECRETS_INBOX)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .map(|d| {
                let secret = self.serializer.deserialize_value(d)?;
                Ok(secret)
            }).collect()
    }

    #[allow(clippy::unused_async)] // Mandated by trait on wasm.
    async fn delete_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
    ) -> Result<()> {
        let range = self.serializer.encode_to_range(keys::SECRETS_INBOX, secret_name.as_str())?;

        self
            .inner
            .transaction_on_one_with_mode(keys::SECRETS_INBOX, IdbTransactionMode::Readwrite)?
            .object_store(keys::SECRETS_INBOX)?
            .delete(&range)?;

        Ok(())
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let key = self.serializer.encode_key(keys::GOSSIP_REQUESTS, key_info.as_key());

        let val = self
            .inner
            .transaction_on_one_with_mode(
                keys::GOSSIP_REQUESTS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::GOSSIP_REQUESTS)?
            .index(keys::GOSSIP_REQUESTS_BY_INFO_INDEX)?
            .get_owned(key)?
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
            .transaction_on_one_with_mode(
                keys::GOSSIP_REQUESTS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::GOSSIP_REQUESTS)?
            .index(keys::GOSSIP_REQUESTS_UNSENT_INDEX)?
            .get_all()?
            .await?
            .iter()
            .filter_map(|val| self.deserialize_gossip_request(val).ok())
            .collect();

        Ok(results)
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        let jskey = self.serializer.encode_key(keys::GOSSIP_REQUESTS, request_id);
        let tx = self.inner.transaction_on_one_with_mode(keys::GOSSIP_REQUESTS, IdbTransactionMode::Readwrite)?;
        tx.object_store(keys::GOSSIP_REQUESTS)?.delete_owned(jskey)?;
        tx.await.into_result().map_err(|e| e.into())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        let key = {
            let tx = self
                .inner
                .transaction_on_one_with_mode(keys::BACKUP_KEYS, IdbTransactionMode::Readonly)?;
            let store = tx.object_store(keys::BACKUP_KEYS)?;

            let backup_version = store
                .get(&JsValue::from_str(keys::BACKUP_KEY_V1))?
                .await?
                .map(|i| self.serializer.deserialize_value(i))
                .transpose()?;

            let decryption_key = store
                .get(&JsValue::from_str(keys::RECOVERY_KEY_V1))?
                .await?
                .map(|i| self.serializer.deserialize_value(i))
                .transpose()?;

            BackupKeys { backup_version, decryption_key }
        };

        Ok(key)
    }

    async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEvent>> {
        let key = self.serializer.encode_key(keys::DIRECT_WITHHELD_INFO, (session_id, room_id));
        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(
                keys::DIRECT_WITHHELD_INFO,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::DIRECT_WITHHELD_INFO)?
            .get(&key)?
            .await?
        {
            let info = self.serializer.deserialize_value(pickle)?;
            Ok(Some(info))
        } else {
            Ok(None)
        }
    }

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>> {
        let key = self.serializer.encode_key(keys::ROOM_SETTINGS, room_id);
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::ROOM_SETTINGS, IdbTransactionMode::Readonly)?
            .object_store(keys::ROOM_SETTINGS)?
            .get(&key)?
            .await?
            .map(|v| self.serializer.deserialize_value(v))
            .transpose()?)
    }

    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readonly)?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(key))?
            .await?
            .map(|v| self.serializer.deserialize_value(v))
            .transpose()?)
    }

    #[allow(clippy::unused_async)] // Mandated by trait on wasm.
    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<()> {
        self
            .inner
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readwrite)?
            .object_store(keys::CORE)?
            .put_key_val(&JsValue::from_str(key), &self.serializer.serialize_value(&value)?)?;
        Ok(())
    }

    #[allow(clippy::unused_async)] // Mandated by trait on wasm.
    async fn remove_custom_value(&self, key: &str) -> Result<()> {
        self
            .inner
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readwrite)?
            .object_store(keys::CORE)?
            .delete(&JsValue::from_str(key))?;
        Ok(())
    }

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool> {
        // As of 2023-06-23, the code below hasn't been tested yet.
        let key = JsValue::from_str(key);
        let txn = self
            .inner
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readwrite)?;
        let object_store = txn
            .object_store(keys::CORE)?;

        #[derive(serde::Deserialize, serde::Serialize)]
        struct Lease {
            holder: String,
            expiration_ts: u64,
        }

        let now_ts: u64 = MilliSecondsSinceUnixEpoch::now().get().into();
        let expiration_ts = now_ts + lease_duration_ms as u64;

        let prev = object_store.get(&key)?.await?;
        match prev {
            Some(prev) => {
                let lease: Lease = self.serializer.deserialize_value(prev)?;
                if lease.holder == holder || lease.expiration_ts < now_ts {
                    object_store.put_key_val(&key, &self.serializer.serialize_value(&Lease { holder: holder.to_owned(), expiration_ts })?)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            None => {
                object_store.put_key_val(&key, &self.serializer.serialize_value(&Lease { holder: holder.to_owned(), expiration_ts })?)?;
                Ok(true)
            }
        }
    }

    #[allow(clippy::unused_async)] // Mandated by trait on wasm.
    async fn clear_caches(&self) {
        self.session_cache.clear()
        // We don't need to clear `static_account` as it only contains immutable data
        // therefore cannot get out of sync with the underlying store.
    }
}

impl Drop for IndexeddbCryptoStore {
    fn drop(&mut self) {
        // Must release the database access manually as it's not done when
        // dropping it.
        self.inner.close();
    }
}

/// Open the meta store.
///
/// The meta store contains details about the encryption of the main store.
async fn open_meta_db(prefix: &str) -> Result<IdbDatabase, IndexeddbCryptoStoreError> {
    let name = format!("{prefix:0}::matrix-sdk-crypto-meta");

    debug!("IndexedDbCryptoStore: Opening meta-store {name}");
    let mut db_req: OpenDbRequest = IdbDatabase::open_u32(&name, 1)?;
    db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
        let old_version = evt.old_version() as u32;
        if old_version < 1 {
            // migrating to version 1
            let db = evt.db();

            db.create_object_store("matrix-sdk-crypto")?;
        }
        Ok(())
    }));

    Ok(db_req.await?)
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
    meta_db: &IdbDatabase,
) -> Result<Option<Vec<u8>>, IndexeddbCryptoStoreError> {
    let tx: IdbTransaction<'_> =
        meta_db.transaction_on_one_with_mode("matrix-sdk-crypto", IdbTransactionMode::Readonly)?;
    let ob = tx.object_store("matrix-sdk-crypto")?;

    let store_cipher: Option<Vec<u8>> = ob
        .get(&JsValue::from_str(keys::STORE_CIPHER))?
        .await?
        .map(|k| k.into_serde())
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
    db: &IdbDatabase,
    export: &Vec<u8>,
) -> Result<(), IndexeddbCryptoStoreError> {
    let tx: IdbTransaction<'_> =
        db.transaction_on_one_with_mode("matrix-sdk-crypto", IdbTransactionMode::Readwrite)?;
    let ob = tx.object_store("matrix-sdk-crypto")?;

    ob.put_key_val(&JsValue::from_str(keys::STORE_CIPHER), &JsValue::from_serde(&export)?)?;
    tx.await.into_result()?;
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
    db: &IdbDatabase,
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
            debug!("IndexedDbCryptoStore: Migrating passphrase-encrypted store cipher to key-encryption");

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
    object_store: IdbObjectStore<'_>,
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
        let after_latest_key =
            IdbKeyRange::lower_bound_with_open(&latest_key, true).expect("Key was not valid!");
        let cursor = object_store.open_cursor_with_range(&after_latest_key)?.await?;

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
    cursor: Option<IdbCursorWithValue<'_, Q>>,
    batch_size: usize,
    f: &F,
    result: &mut Vec<R>,
) -> Result<Option<JsValue>>
where
    F: Fn(JsValue) -> Result<R>,
    Q: IdbQuerySource,
{
    let Some(cursor) = cursor else {
        // Cursor was None - there are no more records
        return Ok(None);
    };

    let mut latest_key = None;

    for _ in 0..batch_size {
        // Process the record
        let processed = f(cursor.value());
        if let Ok(processed) = processed {
            result.push(processed);
        }
        // else processing failed: don't return this record at all

        // Remember that we have processed this record, so if we hit
        // the end of the batch, the next batch can start after this one
        if let Some(key) = cursor.key() {
            latest_key = Some(key);
        }

        // Move on to the next record
        let more_records = cursor.continue_cursor()?.await?;
        if !more_records {
            return Ok(None);
        }
    }

    // We finished the batch but there are more records -
    // return the key of the last one we processed
    Ok(latest_key)
}

/// The objects we store in the gossip_requests indexeddb object store
#[derive(Debug, serde::Serialize, serde::Deserialize)]
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
        with = "crate::serialize_bool_for_indexeddb"
    )]
    unsent: bool,
}

/// The objects we store in the inbound_group_sessions2 indexeddb object store
#[derive(serde::Serialize, serde::Deserialize)]
struct InboundGroupSessionIndexedDbObject {
    /// Possibly encrypted
    /// [`matrix_sdk_crypto::olm::group_sessions::PickledInboundGroupSession`]
    pickled_session: MaybeEncrypted,

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
        with = "crate::serialize_bool_for_indexeddb"
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
}

impl InboundGroupSessionIndexedDbObject {
    pub fn new(pickled_session: MaybeEncrypted, needs_backup: bool) -> Self {
        Self { pickled_session, needs_backup, backed_up_to: -1 }
    }
}

#[cfg(test)]
mod unit_tests {
    use matrix_sdk_store_encryption::EncryptedValueBase64;

    use super::InboundGroupSessionIndexedDbObject;
    use crate::crypto_store::indexeddb_serializer::MaybeEncrypted;

    #[test]
    fn needs_backup_is_serialized_as_a_u8_in_json() {
        let session_needs_backup = InboundGroupSessionIndexedDbObject::new(
            MaybeEncrypted::Encrypted(EncryptedValueBase64::new(1, "", "")),
            true,
        );

        // Testing the exact JSON here is theoretically flaky in the face of
        // serialization changes in serde_json but it seems unlikely, and it's
        // simple enough to fix if we need to.
        assert!(serde_json::to_string(&session_needs_backup)
            .unwrap()
            .contains(r#""needs_backup":1"#),);
    }

    #[test]
    fn doesnt_need_backup_is_serialized_with_missing_field_in_json() {
        let session_backed_up = InboundGroupSessionIndexedDbObject::new(
            MaybeEncrypted::Encrypted(EncryptedValueBase64::new(1, "", "")),
            false,
        );

        assert!(
            !serde_json::to_string(&session_backed_up).unwrap().contains("needs_backup"),
            "The needs_backup field should be missing!"
        );
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod wasm_unit_tests {
    use matrix_sdk_store_encryption::EncryptedValueBase64;
    use matrix_sdk_test::async_test;
    use wasm_bindgen::JsValue;

    use super::{indexeddb_serializer::MaybeEncrypted, InboundGroupSessionIndexedDbObject};

    fn assert_field_equals(js_value: &JsValue, field: &str, expected: u32) {
        assert_eq!(
            js_sys::Reflect::get(&js_value, &field.into()).unwrap(),
            JsValue::from_f64(expected.into())
        );
    }

    #[async_test]
    fn needs_backup_is_serialized_as_a_u8_in_js() {
        let session_needs_backup = InboundGroupSessionIndexedDbObject::new(
            MaybeEncrypted::Encrypted(EncryptedValueBase64::new(3, "", "")),
            true,
        );

        let js_value = serde_wasm_bindgen::to_value(&session_needs_backup).unwrap();

        assert!(js_value.is_object());
        assert_field_equals(&js_value, "needs_backup", 1);
    }

    #[async_test]
    fn doesnt_need_backup_is_serialized_with_missing_field_in_js() {
        let session_backed_up = InboundGroupSessionIndexedDbObject::new(
            MaybeEncrypted::Encrypted(EncryptedValueBase64::new(3, "", "")),
            false,
        );

        let js_value = serde_wasm_bindgen::to_value(&session_backed_up).unwrap();

        assert!(!js_sys::Reflect::has(&js_value, &"needs_backup".into()).unwrap());
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use matrix_sdk_crypto::cryptostore_integration_tests;

    use super::IndexeddbCryptoStore;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    async fn get_store(name: &str, passphrase: Option<&str>) -> IndexeddbCryptoStore {
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

#[cfg(all(test, target_arch = "wasm32"))]
mod encrypted_tests {
    use matrix_sdk_crypto::{
        cryptostore_integration_tests,
        olm::Account,
        store::{CryptoStore, PendingChanges},
        vodozemac::base64_encode,
    };
    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id};

    use super::IndexeddbCryptoStore;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    async fn get_store(name: &str, passphrase: Option<&str>) -> IndexeddbCryptoStore {
        let pass = passphrase.unwrap_or(name);
        // make sure to use a different store name than the equivalent unencrypted test
        let store_name = name.to_owned() + "_enc";
        IndexeddbCryptoStore::open_with_passphrase(&store_name, pass)
            .await
            .expect("Can't create a passphrase protected store")
    }
    cryptostore_integration_tests!();

    /// Test that we can migrate a store created with a passphrase, to being
    /// encrypted with a key instead.
    #[async_test]
    async fn migrate_passphrase_to_key() {
        let store_name = "test_migrate_passphrase_to_key";
        let passdata: [u8; 32] = rand::random();
        let b64_passdata = base64_encode(passdata);

        // Initialise the store with some account data
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
