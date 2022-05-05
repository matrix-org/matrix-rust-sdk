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
    collections::{HashMap, HashSet},
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use dashmap::DashSet;
use indexed_db_futures::prelude::*;
use matrix_sdk_base::locks::Mutex;
use matrix_sdk_crypto::{
    olm::{
        IdentityKeys, InboundGroupSession, OlmMessageHash, OutboundGroupSession,
        PrivateCrossSigningIdentity, Session,
    },
    store::{
        caches::SessionStore, BackupKeys, Changes, CryptoStore, CryptoStoreError, RoomKeyCounts,
    },
    GossipRequest, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentities, SecretInfo,
};
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{DeviceId, OwnedDeviceId, OwnedUserId, RoomId, TransactionId, UserId};
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsValue;

use crate::safe_encode::SafeEncode;

#[allow(non_snake_case)]
mod KEYS {
    // STORES
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

    // KEYS
    pub const STORE_CIPHER: &str = "store_cipher";
    pub const ACCOUNT: &str = "account";
    pub const PRIVATE_IDENTITY: &str = "private_identity";

    // BACKUP v1
    pub const BACKUP_KEYS: &str = "backup_keys";
    pub const BACKUP_KEY_V1: &str = "backup_key_v1";
    pub const RECOVERY_KEY_V1: &str = "recovery_key_v1";
}

/// An in-memory only store that will forget all the E2EE key once it's dropped.
pub struct IndexeddbStore {
    account_info: Arc<RwLock<Option<AccountInfo>>>,
    name: String,
    pub(crate) inner: IdbDatabase,

    store_cipher: Option<Arc<StoreCipher>>,

    session_cache: SessionStore,
    tracked_users_cache: Arc<DashSet<OwnedUserId>>,
    users_for_key_query_cache: Arc<DashSet<OwnedUserId>>,
}

impl std::fmt::Debug for IndexeddbStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbStore").field("name", &self.name).finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbStoreError {
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

impl From<indexed_db_futures::web_sys::DomException> for IndexeddbStoreError {
    fn from(frm: indexed_db_futures::web_sys::DomException) -> IndexeddbStoreError {
        IndexeddbStoreError::DomException {
            name: frm.name(),
            message: frm.message(),
            code: frm.code(),
        }
    }
}

impl From<IndexeddbStoreError> for CryptoStoreError {
    fn from(frm: IndexeddbStoreError) -> CryptoStoreError {
        match frm {
            IndexeddbStoreError::Json(e) => CryptoStoreError::Serialization(e),
            IndexeddbStoreError::CryptoStoreError(e) => e,
            _ => CryptoStoreError::Backend(Box::new(frm)),
        }
    }
}

type Result<A, E = IndexeddbStoreError> = std::result::Result<A, E>;

#[derive(Clone, Debug)]
pub struct AccountInfo {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    identity_keys: Arc<IdentityKeys>,
}

impl IndexeddbStore {
    pub(crate) async fn open_with_store_cipher(
        prefix: String,
        store_cipher: Option<Arc<StoreCipher>>,
    ) -> Result<Self> {
        let name = format!("{:0}::matrix-sdk-crypto", prefix);

        // Open my_db v1
        let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&name, 1.0)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            if evt.old_version() < 1.0 {
                // migrating to version 1
                let db = evt.db();

                db.create_object_store(KEYS::CORE)?;
                db.create_object_store(KEYS::SESSION)?;

                db.create_object_store(KEYS::INBOUND_GROUP_SESSIONS)?;
                db.create_object_store(KEYS::OUTBOUND_GROUP_SESSIONS)?;
                db.create_object_store(KEYS::TRACKED_USERS)?;
                db.create_object_store(KEYS::OLM_HASHES)?;
                db.create_object_store(KEYS::DEVICES)?;

                db.create_object_store(KEYS::IDENTITIES)?;
                db.create_object_store(KEYS::OUTGOING_SECRET_REQUESTS)?;
                db.create_object_store(KEYS::UNSENT_SECRET_REQUESTS)?;
                db.create_object_store(KEYS::SECRET_REQUESTS_BY_INFO)?;

                db.create_object_store(KEYS::BACKUP_KEYS)?;
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
            tracked_users_cache: DashSet::new().into(),
            users_for_key_query_cache: DashSet::new().into(),
        })
    }

    /// Open a new IndexeddbStore with default name and no passphrase
    pub async fn open() -> Result<Self> {
        IndexeddbStore::open_with_store_cipher("crypto".to_owned(), None).await
    }

    /// Open a new IndexeddbStore with given name and passphrase
    pub async fn open_with_passphrase(prefix: String, passphrase: &str) -> Result<Self> {
        let name = format!("{:0}::matrix-sdk-crypto-meta", prefix);

        let mut db_req: OpenDbRequest = IdbDatabase::open_f64(&name, 1.0)?;
        db_req.set_on_upgrade_needed(Some(|evt: &IdbVersionChangeEvent| -> Result<(), JsValue> {
            if evt.old_version() < 1.0 {
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
            .get(&JsValue::from_str(KEYS::STORE_CIPHER))?
            .await?
            .map(|k| k.into_serde())
            .transpose()?;

        let store_cipher = match store_cipher {
            Some(key) => StoreCipher::import(passphrase, &key)
                .map_err(|_| CryptoStoreError::UnpicklingError)?,
            None => {
                let key = StoreCipher::new().map_err(|e| CryptoStoreError::Backend(Box::new(e)))?;
                let encrypted =
                    key.export(passphrase).map_err(|e| CryptoStoreError::Backend(Box::new(e)))?;

                let tx: IdbTransaction<'_> = db.transaction_on_one_with_mode(
                    "matrix-sdk-crypto",
                    IdbTransactionMode::Readwrite,
                )?;
                let ob = tx.object_store("matrix-sdk-crypto")?;

                ob.put_key_val(
                    &JsValue::from_str(KEYS::STORE_CIPHER),
                    &JsValue::from_serde(&encrypted)?,
                )?;
                tx.await.into_result()?;
                key
            }
        };

        IndexeddbStore::open_with_store_cipher(prefix, Some(store_cipher.into())).await
    }

    /// Open a new IndexeddbStore with given name and no passphrase
    pub async fn open_with_name(name: String) -> Result<Self> {
        IndexeddbStore::open_with_store_cipher(name, None).await
    }

    fn serialize_value(&self, value: &impl Serialize) -> Result<JsValue, CryptoStoreError> {
        if let Some(key) = &self.store_cipher {
            let value =
                key.encrypt_value(value).map_err(|e| CryptoStoreError::Backend(Box::new(e)))?;

            Ok(JsValue::from_serde(&value)?)
        } else {
            Ok(JsValue::from_serde(&value)?)
        }
    }

    fn deserialize_value<T: for<'b> Deserialize<'b>>(
        &self,
        value: JsValue,
    ) -> Result<T, CryptoStoreError> {
        if let Some(key) = &self.store_cipher {
            let value: Vec<u8> = value.into_serde()?;
            key.decrypt_value(&value).map_err(|e| CryptoStoreError::Backend(Box::new(e)))
        } else {
            Ok(value.into_serde()?)
        }
    }

    fn get_account_info(&self) -> Option<AccountInfo> {
        self.account_info.read().unwrap().clone()
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        let mut stores: Vec<&str> = [
            (changes.account.is_some() || changes.private_identity.is_some(), KEYS::CORE),
            (changes.recovery_key.is_some() || changes.backup_version.is_some(), KEYS::BACKUP_KEYS),
            (!changes.sessions.is_empty(), KEYS::SESSION),
            (
                !changes.devices.new.is_empty()
                    || !changes.devices.changed.is_empty()
                    || !changes.devices.deleted.is_empty(),
                KEYS::DEVICES,
            ),
            (
                !changes.identities.new.is_empty() || !changes.identities.changed.is_empty(),
                KEYS::IDENTITIES,
            ),
            (!changes.inbound_group_sessions.is_empty(), KEYS::INBOUND_GROUP_SESSIONS),
            (!changes.outbound_group_sessions.is_empty(), KEYS::OUTBOUND_GROUP_SESSIONS),
            (!changes.message_hashes.is_empty(), KEYS::OLM_HASHES),
        ]
        .iter()
        .filter_map(|(id, key)| if *id { Some(*key) } else { None })
        .collect();

        if !changes.key_requests.is_empty() {
            stores.extend([
                KEYS::SECRET_REQUESTS_BY_INFO,
                KEYS::UNSENT_SECRET_REQUESTS,
                KEYS::OUTGOING_SECRET_REQUESTS,
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
            if let Some(i) = changes.private_identity { Some(i.pickle().await?) } else { None };

        let recovery_key_pickle = changes.recovery_key;
        let backup_version = changes.backup_version;

        if let Some(a) = &account_pickle {
            tx.object_store(KEYS::CORE)?
                .put_key_val(&JsValue::from_str(KEYS::ACCOUNT), &self.serialize_value(&a)?)?;
        }

        if let Some(i) = &private_identity_pickle {
            tx.object_store(KEYS::CORE)?.put_key_val(
                &JsValue::from_str(KEYS::PRIVATE_IDENTITY),
                &self.serialize_value(i)?,
            )?;
        }

        if let Some(a) = &recovery_key_pickle {
            tx.object_store(KEYS::BACKUP_KEYS)?.put_key_val(
                &JsValue::from_str(KEYS::RECOVERY_KEY_V1),
                &self.serialize_value(&a)?,
            )?;
        }

        if let Some(a) = &backup_version {
            tx.object_store(KEYS::BACKUP_KEYS)?
                .put_key_val(&JsValue::from_str(KEYS::BACKUP_KEY_V1), &self.serialize_value(&a)?)?;
        }

        if !changes.sessions.is_empty() {
            let sessions = tx.object_store(KEYS::SESSION)?;

            for session in &changes.sessions {
                let sender_key = session.sender_key().to_base64();
                let session_id = session.session_id();

                let pickle = session.pickle().await;
                let key = (&sender_key, session_id).encode();

                sessions.put_key_val(&key, &self.serialize_value(&pickle)?)?;
            }
        }

        if !changes.inbound_group_sessions.is_empty() {
            let sessions = tx.object_store(KEYS::INBOUND_GROUP_SESSIONS)?;

            for session in changes.inbound_group_sessions {
                let room_id = session.room_id();
                let sender_key = session.sender_key();
                let session_id = session.session_id();
                let key = (room_id, sender_key, session_id).encode();
                let pickle = session.pickle().await;

                sessions.put_key_val(&key, &self.serialize_value(&pickle)?)?;
            }
        }

        if !changes.outbound_group_sessions.is_empty() {
            let sessions = tx.object_store(KEYS::OUTBOUND_GROUP_SESSIONS)?;

            for session in changes.outbound_group_sessions {
                let room_id = session.room_id();
                let pickle = session.pickle().await;
                sessions.put_key_val(&room_id.encode(), &self.serialize_value(&pickle)?)?;
            }
        }

        let device_changes = changes.devices;
        let identity_changes = changes.identities;
        let olm_hashes = changes.message_hashes;
        let key_requests = changes.key_requests;

        if !device_changes.new.is_empty() || !device_changes.changed.is_empty() {
            let device_store = tx.object_store(KEYS::DEVICES)?;
            for device in device_changes.new.iter().chain(&device_changes.changed) {
                let key = (device.user_id(), device.device_id()).encode();
                let device = self.serialize_value(&device)?;

                device_store.put_key_val(&key, &device)?;
            }
        }

        if !device_changes.deleted.is_empty() {
            let device_store = tx.object_store(KEYS::DEVICES)?;

            for device in &device_changes.deleted {
                let key = (device.user_id(), device.device_id()).encode();
                device_store.delete(&key)?;
            }
        }

        if !identity_changes.changed.is_empty() || !identity_changes.new.is_empty() {
            let identities = tx.object_store(KEYS::IDENTITIES)?;
            for identity in identity_changes.changed.iter().chain(&identity_changes.new) {
                identities
                    .put_key_val(&identity.user_id().encode(), &self.serialize_value(&identity)?)?;
            }
        }

        if !olm_hashes.is_empty() {
            let hashes = tx.object_store(KEYS::OLM_HASHES)?;
            for hash in &olm_hashes {
                hashes.put_key_val(&(&hash.sender_key, &hash.hash).encode(), &JsValue::TRUE)?;
            }
        }

        if !key_requests.is_empty() {
            let secret_requests_by_info = tx.object_store(KEYS::SECRET_REQUESTS_BY_INFO)?;
            let unsent_secret_requests = tx.object_store(KEYS::UNSENT_SECRET_REQUESTS)?;
            let outgoing_secret_requests = tx.object_store(KEYS::OUTGOING_SECRET_REQUESTS)?;
            for key_request in &key_requests {
                let key_request_id = key_request.request_id.encode();
                secret_requests_by_info
                    .put_key_val(&key_request.info.as_key().encode(), &key_request_id)?;

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

        tx.await.into_result()?;

        // all good, let's update our caches:indexeddb
        for session in changes.sessions {
            self.session_cache.add(session).await;
        }

        Ok(())
    }

    async fn load_tracked_users(&self) -> Result<()> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(KEYS::TRACKED_USERS, IdbTransactionMode::Readonly)?;
        let os = tx.object_store(KEYS::TRACKED_USERS)?;
        let user_ids = os.get_all_keys()?.await?;
        for user_id in user_ids.iter() {
            let dirty: bool =
                !matches!(os.get(&user_id)?.await?.map(|v| v.into_serde()), Some(Ok(false)));
            let user = match user_id.as_string().map(UserId::parse) {
                Some(Ok(user)) => user,
                _ => continue,
            };
            self.tracked_users_cache.insert(user.clone());

            if dirty {
                self.users_for_key_query_cache.insert(user);
            }
        }

        Ok(())
    }

    async fn load_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;
        if let Some(value) = self
            .inner
            .transaction_on_one_with_mode(
                KEYS::OUTBOUND_GROUP_SESSIONS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(KEYS::OUTBOUND_GROUP_SESSIONS)?
            .get(&room_id.encode())?
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
    async fn get_outgoing_key_request_helper(&self, key: &str) -> Result<Option<GossipRequest>> {
        // in this internal we expect key to already be escaped.
        let jskey = JsValue::from_str(key);
        let dbs = [KEYS::OUTGOING_SECRET_REQUESTS, KEYS::UNSENT_SECRET_REQUESTS];
        let tx = self.inner.transaction_on_multi_with_mode(&dbs, IdbTransactionMode::Readonly)?;

        let request = tx
            .object_store(KEYS::OUTGOING_SECRET_REQUESTS)?
            .get(&jskey)?
            .await?
            .map(|i| self.deserialize_value(i))
            .transpose()?;

        Ok(match request {
            None => tx
                .object_store(KEYS::UNSENT_SECRET_REQUESTS)?
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
            .transaction_on_one_with_mode(KEYS::CORE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::CORE)?
            .get(&JsValue::from_str(KEYS::ACCOUNT))?
            .await?
        {
            self.load_tracked_users().await?;

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

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(KEYS::CORE, IdbTransactionMode::Readonly)?
            .object_store(KEYS::CORE)?
            .get(&JsValue::from_str(KEYS::PRIVATE_IDENTITY))?
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
            let range =
                sender_key.encode_to_range().map_err(|e| IndexeddbStoreError::DomException {
                    code: 0,
                    name: "IdbKeyRangeMakeError".to_owned(),
                    message: e,
                })?;
            let sessions: Vec<Session> = self
                .inner
                .transaction_on_one_with_mode(KEYS::SESSION, IdbTransactionMode::Readonly)?
                .object_store(KEYS::SESSION)?
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
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let key = (room_id, sender_key, session_id).encode();
        if let Some(pickle) = self
            .inner
            .transaction_on_one_with_mode(
                KEYS::INBOUND_GROUP_SESSIONS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(KEYS::INBOUND_GROUP_SESSIONS)?
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
                KEYS::INBOUND_GROUP_SESSIONS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(KEYS::INBOUND_GROUP_SESSIONS)?
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

    fn tracked_users(&self) -> HashSet<OwnedUserId> {
        self.tracked_users_cache.to_owned().iter().map(|u| u.clone()).collect()
    }

    fn users_for_key_query(&self) -> HashSet<OwnedUserId> {
        self.users_for_key_query_cache.iter().map(|u| u.clone()).collect()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        let already_added = self.tracked_users_cache.insert(user.to_owned());

        if dirty {
            self.users_for_key_query_cache.insert(user.to_owned());
        } else {
            self.users_for_key_query_cache.remove(user);
        }

        let tx = self
            .inner
            .transaction_on_one_with_mode(KEYS::TRACKED_USERS, IdbTransactionMode::Readwrite)?;
        let os = tx.object_store(KEYS::TRACKED_USERS)?;

        os.put_key_val(
            &JsValue::from_str(user.as_str()),
            &match dirty {
                true => JsValue::TRUE,
                false => JsValue::FALSE,
            },
        )?;

        tx.await.into_result()?;
        Ok(already_added)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let key = (user_id, device_id).encode();
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::DEVICES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::DEVICES)?
            .get(&key)?
            .await?
            .map(|i| self.deserialize_value(i))
            .transpose()?)
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        let range = user_id.encode_to_range().map_err(|e| IndexeddbStoreError::DomException {
            code: 0,
            name: "IdbKeyRangeMakeError".to_owned(),
            message: e,
        })?;
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::DEVICES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::DEVICES)?
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
            .transaction_on_one_with_mode(KEYS::IDENTITIES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::IDENTITIES)?
            .get(&user_id.encode())?
            .await?
            .map(|i| self.deserialize_value(i))
            .transpose()?)
    }

    async fn is_message_known(&self, hash: &OlmMessageHash) -> Result<bool> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(KEYS::OLM_HASHES, IdbTransactionMode::Readonly)?
            .object_store(KEYS::OLM_HASHES)?
            .get(&(&hash.sender_key, &hash.hash).encode())?
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
                KEYS::SECRET_REQUESTS_BY_INFO,
                IdbTransactionMode::Readonly,
            )?
            .object_store(KEYS::SECRET_REQUESTS_BY_INFO)?
            .get(&key_info.as_key().encode())?
            .await?
            .and_then(|i| i.as_string());
        if let Some(id) = id {
            self.get_outgoing_key_request_helper(&id).await
        } else {
            Ok(None)
        }
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        Ok(self
            .inner
            .transaction_on_one_with_mode(
                KEYS::UNSENT_SECRET_REQUESTS,
                IdbTransactionMode::Readonly,
            )?
            .object_store(KEYS::UNSENT_SECRET_REQUESTS)?
            .get_all()?
            .await?
            .iter()
            .filter_map(|i| self.deserialize_value(i).ok())
            .collect())
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        let jskey = request_id.as_str().encode();
        let dbs = [
            KEYS::OUTGOING_SECRET_REQUESTS,
            KEYS::UNSENT_SECRET_REQUESTS,
            KEYS::SECRET_REQUESTS_BY_INFO,
        ];
        let tx = self.inner.transaction_on_multi_with_mode(&dbs, IdbTransactionMode::Readwrite)?;

        let request: Option<GossipRequest> = tx
            .object_store(KEYS::OUTGOING_SECRET_REQUESTS)?
            .get(&jskey)?
            .await?
            .map(|i| self.deserialize_value(i))
            .transpose()?;

        let request = match request {
            None => tx
                .object_store(KEYS::UNSENT_SECRET_REQUESTS)?
                .get(&jskey)?
                .await?
                .map(|i| self.deserialize_value(i))
                .transpose()?,
            Some(request) => Some(request),
        };

        if let Some(inner) = request {
            tx.object_store(KEYS::SECRET_REQUESTS_BY_INFO)?
                .delete(&inner.info.as_key().encode())?;
        }

        tx.object_store(KEYS::UNSENT_SECRET_REQUESTS)?.delete(&jskey)?;
        tx.object_store(KEYS::OUTGOING_SECRET_REQUESTS)?.delete(&jskey)?;

        tx.await.into_result().map_err(|e| e.into())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        let key = {
            let tx = self
                .inner
                .transaction_on_one_with_mode(KEYS::BACKUP_KEYS, IdbTransactionMode::Readonly)?;
            let store = tx.object_store(KEYS::BACKUP_KEYS)?;

            let backup_version = store
                .get(&JsValue::from_str(KEYS::BACKUP_KEY_V1))?
                .await?
                .map(|i| self.deserialize_value(i))
                .transpose()?;

            let recovery_key = store
                .get(&JsValue::from_str(KEYS::RECOVERY_KEY_V1))?
                .await?
                .map(|i| self.deserialize_value(i))
                .transpose()?;

            BackupKeys { backup_version, recovery_key }
        };

        Ok(key)
    }
}

#[async_trait(?Send)]
impl CryptoStore for IndexeddbStore {
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>, CryptoStoreError> {
        self.load_account().await.map_err(|e| e.into())
    }

    async fn save_account(&self, account: ReadOnlyAccount) -> Result<(), CryptoStoreError> {
        self.save_changes(Changes { account: Some(account), ..Default::default() })
            .await
            .map_err(|e| e.into())
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>, CryptoStoreError> {
        self.load_identity().await.map_err(|e| e.into())
    }

    async fn save_changes(&self, changes: Changes) -> Result<(), CryptoStoreError> {
        self.save_changes(changes).await.map_err(|e| e.into())
    }

    async fn get_sessions(
        &self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Session>>>>, CryptoStoreError> {
        self.get_sessions(sender_key).await.map_err(|e| e.into())
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>, CryptoStoreError> {
        self.get_inbound_group_session(room_id, sender_key, session_id).await.map_err(|e| e.into())
    }

    async fn get_inbound_group_sessions(
        &self,
    ) -> Result<Vec<InboundGroupSession>, CryptoStoreError> {
        self.get_inbound_group_sessions().await.map_err(|e| e.into())
    }

    async fn get_outbound_group_sessions(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>, CryptoStoreError> {
        self.load_outbound_group_session(room_id).await.map_err(|e| e.into())
    }

    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts, CryptoStoreError> {
        self.inbound_group_session_counts().await.map_err(|e| e.into())
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>, CryptoStoreError> {
        self.inbound_group_sessions_for_backup(limit).await.map_err(|e| e.into())
    }

    async fn reset_backup_state(&self) -> Result<(), CryptoStoreError> {
        self.reset_backup_state().await.map_err(|e| e.into())
    }

    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        self.tracked_users_cache.contains(user_id)
    }

    fn has_users_for_key_query(&self) -> bool {
        !self.users_for_key_query_cache.is_empty()
    }

    fn tracked_users(&self) -> HashSet<OwnedUserId> {
        self.tracked_users()
    }

    fn users_for_key_query(&self) -> HashSet<OwnedUserId> {
        self.users_for_key_query()
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys, CryptoStoreError> {
        self.load_backup_keys().await.map_err(|e| e.into())
    }

    async fn update_tracked_user(
        &self,
        user: &UserId,
        dirty: bool,
    ) -> Result<bool, CryptoStoreError> {
        self.update_tracked_user(user, dirty).await.map_err(|e| e.into())
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>, CryptoStoreError> {
        self.get_device(user_id, device_id).await.map_err(|e| e.into())
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>, CryptoStoreError> {
        self.get_user_devices(user_id).await.map_err(|e| e.into())
    }

    async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> Result<Option<ReadOnlyUserIdentities>, CryptoStoreError> {
        self.get_user_identity(user_id).await.map_err(|e| e.into())
    }

    async fn is_message_known(&self, hash: &OlmMessageHash) -> Result<bool, CryptoStoreError> {
        self.is_message_known(hash).await.map_err(|e| e.into())
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>, CryptoStoreError> {
        self.get_outgoing_key_request_helper(request_id.as_str()).await.map_err(|e| e.into())
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>, CryptoStoreError> {
        self.get_secret_request_by_info(key_info).await.map_err(|e| e.into())
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>, CryptoStoreError> {
        self.get_unsent_secret_requests().await.map_err(|e| e.into())
    }

    async fn delete_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<(), CryptoStoreError> {
        self.delete_outgoing_secret_requests(request_id).await.map_err(|e| e.into())
    }
}

#[cfg(test)]
mod tests {
    use super::IndexeddbStore;
    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);
    use matrix_sdk_crypto::cryptostore_integration_tests;

    async fn get_store(name: String, passphrase: Option<&str>) -> IndexeddbStore {
        match passphrase {
            Some(pass) => IndexeddbStore::open_with_passphrase(name, pass)
                .await
                .expect("Can't create a passphrase protected store"),
            None => IndexeddbStore::open_with_name(name)
                .await
                .expect("Can't create store without passphrase"),
        }
    }
    cryptostore_integration_tests! { integration }
}
