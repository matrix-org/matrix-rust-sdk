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
    collections::HashMap,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::prelude::*;
use matrix_sdk_base::locks::Mutex;
use matrix_sdk_crypto::{
    olm::{
        IdentityKeys, InboundGroupSession, OlmMessageHash, OutboundGroupSession,
        PrivateCrossSigningIdentity, Session,
    },
    store::{
        caches::SessionStore, BackupKeys, Changes, CryptoStore, CryptoStoreError, RoomKeyCounts,
        RoomSettings,
    },
    types::events::room_key_withheld::RoomKeyWithheldEvent,
    GossipRequest, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentities, SecretInfo,
    TrackedUser,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{DeviceId, OwnedDeviceId, RoomId, TransactionId, UserId};
use serde::{de::DeserializeOwned, Serialize};
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;

use crate::safe_encode::SafeEncode;

mod keys {
    // stores
    pub const CORE: &str = "core";

    pub const SESSION: &str = "session";
    pub const INBOUND_GROUP_SESSIONS: &str = "inbound_group_sessions";

    pub const OUTBOUND_GROUP_SESSIONS: &str = "outbound_group_sessions";

    pub const TRACKED_USERS: &str = "tracked_users";
    pub const OLM_HASHES: &str = "olm_hashes";

    pub const DEVICES: &str = "devices";
    pub const IDENTITIES: &str = "identities";

    pub const OUTGOING_SECRET_REQUESTS: &str = "outgoing_secret_requests";
    pub const UNSENT_SECRET_REQUESTS: &str = "unsent_secret_requests";
    pub const SECRET_REQUESTS_BY_INFO: &str = "secret_requests_by_info";
    pub const KEY_REQUEST: &str = "key_request";
    pub const ROOM_SETTINGS: &str = "room_settings";

    pub const DIRECT_WITHHELD_INFO: &str = "direct_withheld_info";

    // keys
    pub const STORE_CIPHER: &str = "store_cipher";
    pub const ACCOUNT: &str = "account";
    pub const PRIVATE_IDENTITY: &str = "private_identity";

    // backup v1
    pub const BACKUP_KEYS: &str = "backup_keys";
    pub const BACKUP_KEY_V1: &str = "backup_key_v1";
    pub const RECOVERY_KEY_V1: &str = "recovery_key_v1";
}

/// An in-memory only store that will forget all the E2EE key once it's dropped.
pub struct IndexeddbCryptoStore {
    account_info: Arc<RwLock<Option<AccountInfo>>>,
    name: String,
    pub(crate) inner: IdbDatabase,

    store_cipher: Option<Arc<StoreCipher>>,

    session_cache: SessionStore,
}

impl std::fmt::Debug for IndexeddbCryptoStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbCryptoStore").field("name", &self.name).finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbCryptoStoreError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
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

impl From<indexed_db_futures::web_sys::DomException> for IndexeddbCryptoStoreError {
    fn from(frm: indexed_db_futures::web_sys::DomException) -> IndexeddbCryptoStoreError {
        IndexeddbCryptoStoreError::DomException {
            name: frm.name(),
            message: frm.message(),
            code: frm.code(),
        }
    }
}

impl From<IndexeddbCryptoStoreError> for CryptoStoreError {
    fn from(frm: IndexeddbCryptoStoreError) -> CryptoStoreError {
        match frm {
            IndexeddbCryptoStoreError::Json(e) => CryptoStoreError::Serialization(e),
            IndexeddbCryptoStoreError::CryptoStoreError(e) => e,
            _ => CryptoStoreError::backend(frm),
        }
    }
}

type Result<A, E = IndexeddbCryptoStoreError> = std::result::Result<A, E>;

#[derive(Clone, Debug)]
pub struct AccountInfo {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    identity_keys: Arc<IdentityKeys>,
}

impl IndexeddbCryptoStore {
    pub(crate) async fn open_with_store_cipher(
        prefix: &str,
        store_cipher: Option<Arc<StoreCipher>>,
    ) -> Result<Self> {
        let name = format!("{prefix:0}::matrix-sdk-crypto");

        // Open my_db v1
        let mut db_req: OpenDbRequest = IdbDatabase::open_u32(&name, 3)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            // Even if the web-sys bindings expose the version as a f64, the IndexedDB API
            // works with an unsigned integer.
            // See <https://github.com/rustwasm/wasm-bindgen/issues/1149>
            let old_version = evt.old_version() as u32;

            if old_version < 1 {
                // migrating to version 1
                let db = evt.db();

                db.create_object_store(keys::CORE)?;
                db.create_object_store(keys::SESSION)?;

                db.create_object_store(keys::INBOUND_GROUP_SESSIONS)?;
                db.create_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;
                db.create_object_store(keys::TRACKED_USERS)?;
                db.create_object_store(keys::OLM_HASHES)?;
                db.create_object_store(keys::DEVICES)?;

                db.create_object_store(keys::IDENTITIES)?;
                db.create_object_store(keys::OUTGOING_SECRET_REQUESTS)?;
                db.create_object_store(keys::UNSENT_SECRET_REQUESTS)?;
                db.create_object_store(keys::SECRET_REQUESTS_BY_INFO)?;

                db.create_object_store(keys::BACKUP_KEYS)?;
            }

            if old_version < 2 {
                let db = evt.db();

                // We changed how we store inbound group sessions, the key used to
                // be a trippled of `(room_id, sender_key, session_id)` now it's a
                // tuple of `(room_id, session_id)`
                //
                // Let's just drop the whole object store.
                db.delete_object_store(keys::INBOUND_GROUP_SESSIONS)?;
                db.create_object_store(keys::INBOUND_GROUP_SESSIONS)?;
                db.create_object_store(keys::ROOM_SETTINGS)?;
            }

            if old_version < 3 {
                let db = evt.db();

                // We changed the way we store outbound session.
                // ShareInfo changed from a struct to an enum with struct variant.
                // Let's just discard the existing outbounds
                db.delete_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;
                db.create_object_store(keys::OUTBOUND_GROUP_SESSIONS)?;

                // Support for MSC2399 withheld codes
                db.create_object_store(keys::DIRECT_WITHHELD_INFO)?;
            }

            Ok(())
        }));

        let db: IdbDatabase = db_req.into_future().await?;
        let session_cache = SessionStore::new();

        Ok(Self {
            name,
            session_cache,
            inner: db,
            store_cipher,
            account_info: RwLock::new(None).into(),
        })
    }

    /// Open a new `IndexeddbCryptoStore` with default name and no passphrase
    pub async fn open() -> Result<Self> {
        IndexeddbCryptoStore::open_with_store_cipher("crypto", None).await
    }

    fn encode_key<T>(&self, table_name: &str, key: T) -> JsValue
    where
        T: SafeEncode,
    {
        match &self.store_cipher {
            Some(cipher) => key.encode_secure(table_name, cipher),
            None => key.encode(),
        }
    }

    fn encode_to_range<T>(
        &self,
        table_name: &str,
        key: T,
    ) -> Result<IdbKeyRange, IndexeddbCryptoStoreError>
    where
        T: SafeEncode,
    {
        match &self.store_cipher {
            Some(cipher) => key.encode_to_range_secure(table_name, cipher),
            None => key.encode_to_range(),
        }
        .map_err(|e| IndexeddbCryptoStoreError::DomException {
            code: 0,
            name: "IdbKeyRangeMakeError".to_owned(),
            message: e,
        })
    }

    /// Open a new `IndexeddbCryptoStore` with given name and passphrase
    pub async fn open_with_passphrase(prefix: &str, passphrase: &str) -> Result<Self> {
        let name = format!("{prefix:0}::matrix-sdk-crypto-meta");

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

        let db: IdbDatabase = db_req.into_future().await?;

        let tx: IdbTransaction<'_> =
            db.transaction_on_one_with_mode("matrix-sdk-crypto", IdbTransactionMode::Readonly)?;
        let ob = tx.object_store("matrix-sdk-crypto")?;

        let store_cipher: Option<Vec<u8>> = ob
            .get(&JsValue::from_str(keys::STORE_CIPHER))?
            .await?
            .map(|k| k.into_serde())
            .transpose()?;

        let store_cipher = match store_cipher {
            Some(cipher) => StoreCipher::import(passphrase, &cipher)
                .map_err(|_| CryptoStoreError::UnpicklingError)?,
            None => {
                let cipher = StoreCipher::new().map_err(CryptoStoreError::backend)?;
                #[cfg(not(test))]
                let export = cipher.export(passphrase);
                #[cfg(test)]
                let export = cipher._insecure_export_fast_for_testing(passphrase);

                let tx: IdbTransaction<'_> = db.transaction_on_one_with_mode(
                    "matrix-sdk-crypto",
                    IdbTransactionMode::Readwrite,
                )?;
                let ob = tx.object_store("matrix-sdk-crypto")?;

                ob.put_key_val(
                    &JsValue::from_str(keys::STORE_CIPHER),
                    &JsValue::from_serde(&export.map_err(CryptoStoreError::backend)?)?,
                )?;
                tx.await.into_result()?;
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

    fn serialize_value(&self, value: &impl Serialize) -> Result<JsValue, CryptoStoreError> {
        if let Some(key) = &self.store_cipher {
            let value = key.encrypt_value(value).map_err(CryptoStoreError::backend)?;

            Ok(JsValue::from_serde(&value)?)
        } else {
            Ok(JsValue::from_serde(&value)?)
        }
    }

    fn deserialize_value<T: DeserializeOwned>(
        &self,
        value: JsValue,
    ) -> Result<T, CryptoStoreError> {
        if let Some(key) = &self.store_cipher {
            let value: Vec<u8> = value.into_serde()?;
            key.decrypt_value(&value).map_err(CryptoStoreError::backend)
        } else {
            Ok(value.into_serde()?)
        }
    }

    fn get_account_info(&self) -> Option<AccountInfo> {
        self.account_info.read().unwrap().clone()
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
    async fn save_changes(&self, changes: Changes) -> Result<()> {
        let mut stores: Vec<&str> = [
            (changes.account.is_some() || changes.private_identity.is_some(), keys::CORE),
            (changes.recovery_key.is_some() || changes.backup_version.is_some(), keys::BACKUP_KEYS),
            (!changes.sessions.is_empty(), keys::SESSION),
            (
                !changes.devices.new.is_empty()
                    || !changes.devices.changed.is_empty()
                    || !changes.devices.deleted.is_empty(),
                keys::DEVICES,
            ),
            (
                !changes.identities.new.is_empty() || !changes.identities.changed.is_empty(),
                keys::IDENTITIES,
            ),

            (!changes.inbound_group_sessions.is_empty(), keys::INBOUND_GROUP_SESSIONS),
            (!changes.outbound_group_sessions.is_empty(), keys::OUTBOUND_GROUP_SESSIONS),
            (!changes.message_hashes.is_empty(), keys::OLM_HASHES),
            (!changes.withheld_session_info.is_empty(), keys::DIRECT_WITHHELD_INFO),
            (!changes.room_settings.is_empty(), keys::ROOM_SETTINGS),
        ]
        .iter()
        .filter_map(|(id, key)| if *id { Some(*key) } else { None })
        .collect();

        if !changes.key_requests.is_empty() {
            stores.extend([
                keys::SECRET_REQUESTS_BY_INFO,
                keys::UNSENT_SECRET_REQUESTS,
                keys::OUTGOING_SECRET_REQUESTS,
            ])
        }

        if stores.is_empty() {
            // nothing to do, quit early
            return Ok(());
        }

        let tx =
            self.inner.transaction_on_multi_with_mode(&stores, IdbTransactionMode::Readwrite)?;

        let account_pickle = if let Some(account) = changes.account {
            let account_info = AccountInfo {
                user_id: account.user_id.clone(),
                device_id: account.device_id.clone(),
                identity_keys: account.identity_keys.clone(),
            };

            *self.account_info.write().unwrap() = Some(account_info);
            Some(account.pickle().await)
        } else {
            None
        };

        let private_identity_pickle =
            if let Some(i) = changes.private_identity { Some(i.pickle().await) } else { None };

        let recovery_key_pickle = changes.recovery_key;
        let backup_version = changes.backup_version;

        if let Some(a) = &account_pickle {
            tx.object_store(keys::CORE)?
                .put_key_val(&JsValue::from_str(keys::ACCOUNT), &self.serialize_value(&a)?)?;
        }

        if let Some(i) = &private_identity_pickle {
            tx.object_store(keys::CORE)?.put_key_val(
                &JsValue::from_str(keys::PRIVATE_IDENTITY),
                &self.serialize_value(i)?,
            )?;
        }

        if let Some(a) = &recovery_key_pickle {
            tx.object_store(keys::BACKUP_KEYS)?.put_key_val(
                &JsValue::from_str(keys::RECOVERY_KEY_V1),
                &self.serialize_value(&a)?,
            )?;
        }

        if let Some(a) = &backup_version {
            tx.object_store(keys::BACKUP_KEYS)?
                .put_key_val(&JsValue::from_str(keys::BACKUP_KEY_V1), &self.serialize_value(&a)?)?;
        }

        if !changes.sessions.is_empty() {
            let sessions = tx.object_store(keys::SESSION)?;

            for session in &changes.sessions {
                let sender_key = session.sender_key().to_base64();
                let session_id = session.session_id();

                let pickle = session.pickle().await;
                let key = self.encode_key(keys::SESSION, (&sender_key, session_id));

                sessions.put_key_val(&key, &self.serialize_value(&pickle)?)?;
            }
        }

        if !changes.inbound_group_sessions.is_empty() {
            let sessions = tx.object_store(keys::INBOUND_GROUP_SESSIONS)?;

            for session in changes.inbound_group_sessions {
                let room_id = session.room_id();
                let session_id = session.session_id();
                let key = self.encode_key(keys::INBOUND_GROUP_SESSIONS, (room_id, session_id));
                let pickle = session.pickle().await;

                sessions.put_key_val(&key, &self.serialize_value(&pickle)?)?;
            }
        }

        if !changes.outbound_group_sessions.is_empty() {
            let sessions = tx.object_store(keys::OUTBOUND_GROUP_SESSIONS)?;

            for session in changes.outbound_group_sessions {
                let room_id = session.room_id();
                let pickle = session.pickle().await;
                sessions.put_key_val(
                    &self.encode_key(keys::OUTBOUND_GROUP_SESSIONS, room_id),
                    &self.serialize_value(&pickle)?,
                )?;
            }
        }

        let device_changes = changes.devices;
        let identity_changes = changes.identities;
        let olm_hashes = changes.message_hashes;
        let key_requests = changes.key_requests;
        let withheld_session_info = changes.withheld_session_info;
        let room_settings_changes = changes.room_settings;

        if !device_changes.new.is_empty() || !device_changes.changed.is_empty() {
            let device_store = tx.object_store(keys::DEVICES)?;
            for device in device_changes.new.iter().chain(&device_changes.changed) {
                let key = self.encode_key(keys::DEVICES, (device.user_id(), device.device_id()));
                let device = self.serialize_value(&device)?;

                device_store.put_key_val(&key, &device)?;
            }
        }

        if !device_changes.deleted.is_empty() {
            let device_store = tx.object_store(keys::DEVICES)?;

            for device in &device_changes.deleted {
                let key = self.encode_key(keys::DEVICES, (device.user_id(), device.device_id()));
                device_store.delete(&key)?;
            }
        }

        if !identity_changes.changed.is_empty() || !identity_changes.new.is_empty() {
            let identities = tx.object_store(keys::IDENTITIES)?;
            for identity in identity_changes.changed.iter().chain(&identity_changes.new) {
                identities.put_key_val(
                    &self.encode_key(keys::IDENTITIES, identity.user_id()),
                    &self.serialize_value(&identity)?,
                )?;
            }
        }

        if !olm_hashes.is_empty() {
            let hashes = tx.object_store(keys::OLM_HASHES)?;
            for hash in &olm_hashes {
                hashes.put_key_val(
                    &self.encode_key(keys::OLM_HASHES, (&hash.sender_key, &hash.hash)),
                    &JsValue::TRUE,
                )?;
            }
        }

        if !key_requests.is_empty() {
            let secret_requests_by_info = tx.object_store(keys::SECRET_REQUESTS_BY_INFO)?;
            let unsent_secret_requests = tx.object_store(keys::UNSENT_SECRET_REQUESTS)?;
            let outgoing_secret_requests = tx.object_store(keys::OUTGOING_SECRET_REQUESTS)?;
            for key_request in &key_requests {
                let key_request_id =
                    self.encode_key(keys::KEY_REQUEST, key_request.request_id.as_str());
                secret_requests_by_info.put_key_val(
                    &self.encode_key(keys::KEY_REQUEST, key_request.info.as_key()),
                    &key_request_id,
                )?;

                if key_request.sent_out {
                    unsent_secret_requests.delete(&key_request_id)?;
                    outgoing_secret_requests
                        .put_key_val(&key_request_id, &self.serialize_value(&key_request)?)?;
                } else {
                    outgoing_secret_requests.delete(&key_request_id)?;
                    unsent_secret_requests
                        .put_key_val(&key_request_id, &self.serialize_value(&key_request)?)?;
                }
            }
        }

        if !withheld_session_info.is_empty() {
            let withhelds = tx.object_store(keys::DIRECT_WITHHELD_INFO)?;

            for (room_id, data) in withheld_session_info {
                for (session_id, event) in data {

                    let key = self.encode_key(keys::DIRECT_WITHHELD_INFO, (session_id, &room_id));
                    withhelds.put_key_val(&key, &self.serialize_value(&event)?)?;
                }
            }
        }

        if !room_settings_changes.is_empty() {
            let settings_store = tx.object_store(keys::ROOM_SETTINGS)?;

            for (room_id, settings) in &room_settings_changes {
                let key = self.encode_key(keys::ROOM_SETTINGS, room_id);
                let value = self.serialize_value(&settings)?;
                settings_store.put_key_val(&key, &value)?;
            }
        }

        tx.await.into_result()?;

        // all good, let's update our caches:indexeddb
        for session in changes.sessions {
            self.session_cache.add(session).await;
        }

        Ok(())
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
        let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;
        if let Some(value) = self
            .inner
            .transaction_on_one_with_mode(
                keys::OUTBOUND_GROUP_SESSIONS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::OUTBOUND_GROUP_SESSIONS)?
            .get(&self.encode_key(keys::OUTBOUND_GROUP_SESSIONS, room_id))?
            .await?
        {
            Ok(Some(
                OutboundGroupSession::from_pickle(
                    account_info.device_id,
                    account_info.identity_keys,
                    self.deserialize_value(value)?,
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
        // in this internal we expect key to already be escaped or encrypted
        let jskey = JsValue::from_str(request_id.as_str());
        let dbs = [keys::OUTGOING_SECRET_REQUESTS, keys::UNSENT_SECRET_REQUESTS];
        let tx = self.inner.transaction_on_multi_with_mode(&dbs, IdbTransactionMode::Readonly)?;

        let request = tx
            .object_store(keys::OUTGOING_SECRET_REQUESTS)?
            .get(&jskey)?
            .await?
            .map(|i| self.deserialize_value(i))
            .transpose()?;

        Ok(match request {
            None => tx
                .object_store(keys::UNSENT_SECRET_REQUESTS)?
                .get(&jskey)?
                .await?
                .map(|i| self.deserialize_value(i))
                .transpose()?,
            Some(request) => Some(request),
        })
    }

    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readonly)?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(keys::ACCOUNT))?
            .await?
        {
            let pickle = self.deserialize_value(pickle)?;

            let account = ReadOnlyAccount::from_pickle(pickle).map_err(CryptoStoreError::from)?;

            let account_info = AccountInfo {
                user_id: account.user_id.clone(),
                device_id: account.device_id.clone(),
                identity_keys: account.identity_keys.clone(),
            };

            *self.account_info.write().unwrap() = Some(account_info);

            Ok(Some(account))
        } else {
            Ok(None)
        }
    }

    async fn save_account(&self, account: ReadOnlyAccount) -> Result<()> {
        self.save_changes(Changes { account: Some(account), ..Default::default() })
            .await
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readonly)?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(keys::PRIVATE_IDENTITY))?
            .await?
        {
            let pickle = self.deserialize_value(pickle)?;

            Ok(Some(
                PrivateCrossSigningIdentity::from_pickle(pickle)
                    .await
                    .map_err(|_| CryptoStoreError::UnpicklingError)?,
            ))
        } else {
            Ok(None)
        }
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;

        if self.session_cache.get(sender_key).is_none() {
            let range = self.encode_to_range(keys::SESSION, sender_key)?;
            let sessions: Vec<Session> = self
                .inner
                .transaction_on_one_with_mode(keys::SESSION, IdbTransactionMode::Readonly)?
                .object_store(keys::SESSION)?
                .get_all_with_key(&range)?
                .await?
                .iter()
                .filter_map(|f| match self.deserialize_value(f) {
                    Ok(p) => Some(Session::from_pickle(
                        account_info.user_id.clone(),
                        account_info.device_id.clone(),
                        account_info.identity_keys.clone(),
                        p,
                    )),
                    _ => None,
                })
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
        let key = self.encode_key(keys::INBOUND_GROUP_SESSIONS, (room_id, session_id));
        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(
                keys::INBOUND_GROUP_SESSIONS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::INBOUND_GROUP_SESSIONS)?
            .get(&key)?
            .await?
        {
            let pickle = self.deserialize_value(pickle)?;
            Ok(Some(InboundGroupSession::from_pickle(pickle).map_err(CryptoStoreError::from)?))
        } else {
            Ok(None)
        }
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(
                keys::INBOUND_GROUP_SESSIONS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::INBOUND_GROUP_SESSIONS)?
            .get_all()?
            .await?
            .iter()
            .filter_map(|i| self.deserialize_value(i).ok())
            .filter_map(|p| InboundGroupSession::from_pickle(p).ok())
            .collect())
    }

    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        let all = self.get_inbound_group_sessions().await?;
        let backed_up = all.iter().filter(|s| s.backed_up()).count();

        Ok(RoomKeyCounts { total: all.len(), backed_up })
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        Ok(self
            .get_inbound_group_sessions()
            .await?
            .into_iter()
            .filter(|s| !s.backed_up())
            .take(limit)
            .collect())
    }

    async fn reset_backup_state(&self) -> Result<()> {
        let inbound_group_sessions = self
            .get_inbound_group_sessions()
            .await?
            .into_iter()
            .filter(|s| {
                if s.backed_up() {
                    s.reset_backup_state();
                    true
                } else {
                    false
                }
            })
            .collect::<Vec<_>>();
        if !inbound_group_sessions.is_empty() {
            let changes = Changes { inbound_group_sessions, ..Default::default() };
            self.save_changes(changes).await?;
        }
        Ok(())
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
        let key = self.encode_key(keys::DEVICES, (user_id, device_id));
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::DEVICES, IdbTransactionMode::Readonly)?
            .object_store(keys::DEVICES)?
            .get(&key)?
            .await?
            .map(|i| self.deserialize_value(i))
            .transpose()?)
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        let range = self.encode_to_range(keys::DEVICES, user_id)?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::DEVICES, IdbTransactionMode::Readonly)?
            .object_store(keys::DEVICES)?
            .get_all_with_key(&range)?
            .await?
            .iter()
            .filter_map(|d| {
                let d: ReadOnlyDevice = self.deserialize_value(d).ok()?;
                Some((d.device_id().to_owned(), d))
            })
            .collect::<HashMap<_, _>>())
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::IDENTITIES, IdbTransactionMode::Readonly)?
            .object_store(keys::IDENTITIES)?
            .get(&self.encode_key(keys::IDENTITIES, user_id))?
            .await?
            .map(|i| self.deserialize_value(i))
            .transpose()?)
    }

    async fn is_message_known(&self, hash: &OlmMessageHash) -> Result<bool> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::OLM_HASHES, IdbTransactionMode::Readonly)?
            .object_store(keys::OLM_HASHES)?
            .get(&self.encode_key(keys::OLM_HASHES, (&hash.sender_key, &hash.hash)))?
            .await?
            .is_some())
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let id = self
            .inner
            .transaction_on_one_with_mode(
                keys::SECRET_REQUESTS_BY_INFO,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::SECRET_REQUESTS_BY_INFO)?
            .get(&self.encode_key(keys::KEY_REQUEST, key_info.as_key()))?
            .await?
            .and_then(|i| i.as_string());
        if let Some(id) = id {
            self.get_outgoing_secret_requests(id.as_str().into()).await
        } else {
            Ok(None)
        }
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(
                keys::UNSENT_SECRET_REQUESTS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(keys::UNSENT_SECRET_REQUESTS)?
            .get_all()?
            .await?
            .iter()
            .filter_map(|i| self.deserialize_value(i).ok())
            .collect())
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        let jskey = self.encode_key(keys::KEY_REQUEST, request_id); //.as_str());
        let dbs = [
            keys::OUTGOING_SECRET_REQUESTS,
            keys::UNSENT_SECRET_REQUESTS,
            keys::SECRET_REQUESTS_BY_INFO,
        ];
        let tx = self.inner.transaction_on_multi_with_mode(&dbs, IdbTransactionMode::Readwrite)?;

        let request: Option<GossipRequest> = tx
            .object_store(keys::OUTGOING_SECRET_REQUESTS)?
            .get(&jskey)?
            .await?
            .map(|i| self.deserialize_value(i))
            .transpose()?;

        let request = match request {
            None => tx
                .object_store(keys::UNSENT_SECRET_REQUESTS)?
                .get(&jskey)?
                .await?
                .map(|i| self.deserialize_value(i))
                .transpose()?,
            Some(request) => Some(request),
        };

        if let Some(inner) = request {
            tx.object_store(keys::SECRET_REQUESTS_BY_INFO)?
                .delete(&self.encode_key(keys::KEY_REQUEST, inner.info.as_key()))?;
        }

        tx.object_store(keys::UNSENT_SECRET_REQUESTS)?.delete(&jskey)?;
        tx.object_store(keys::OUTGOING_SECRET_REQUESTS)?.delete(&jskey)?;

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
                .map(|i| self.deserialize_value(i))
                .transpose()?;

            let recovery_key = store
                .get(&JsValue::from_str(keys::RECOVERY_KEY_V1))?
                .await?
                .map(|i| self.deserialize_value(i))
                .transpose()?;

            BackupKeys { backup_version, recovery_key }
        };

        Ok(key)
    }

    async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEvent>> {
        let key = self.encode_key(keys::DIRECT_WITHHELD_INFO, (session_id, room_id));
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
            let info = self.deserialize_value(pickle)?;
            Ok(Some(info))
        } else {
            Ok(None)
        }
    }

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>> {
        let key = self.encode_key(keys::ROOM_SETTINGS, room_id);
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::ROOM_SETTINGS, IdbTransactionMode::Readonly)?
            .object_store(keys::ROOM_SETTINGS)?
            .get(&key)?
            .await?
            .map(|v| self.deserialize_value(v))
            .transpose()?)
    }

    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readonly)?
            .object_store(keys::CORE)?
            .get(&JsValue::from_str(key))?
            .await?
            .map(|v| self.deserialize_value(v))
            .transpose()?)
    }

    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<()> {
        self
            .inner
            .transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readwrite)?
            .object_store(keys::CORE)?
            .put_key_val(&JsValue::from_str(key), &self.serialize_value(&value)?)?;
        Ok(())
    }
}

impl Drop for IndexeddbCryptoStore {
    fn drop(&mut self) {
        // Must release the database access manually as it's not done when
        // dropping it.
        self.inner.close();
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
#[rustfmt::skip]
mod encrypted_tests {
    use super::IndexeddbCryptoStore;
    use matrix_sdk_crypto::cryptostore_integration_tests;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    async fn get_store(name: &str, passphrase: Option<&str>) -> IndexeddbCryptoStore {
        let pass = passphrase.unwrap_or(name);
        IndexeddbCryptoStore::open_with_passphrase(name, pass)
            .await
            .expect("Can't create a passphrase protected store")
    }

    // FIXME: the tests pass, if run one by one, but run all together locally,
    //        as well as CI fails... see matrix-org/matrix-rust-sdk#661
    //     cryptostore_integration_tests!();
}
