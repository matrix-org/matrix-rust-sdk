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
    convert::{TryFrom, TryInto},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use dashmap::DashSet;
use matrix_sdk_common::{async_trait, locks::Mutex, uuid};
use olm_rs::{account::IdentityKeys, PicklingMode};
use ruma::{
    events::{room_key_request::RequestedKeyInfo, secret::request::SecretName},
    DeviceId, DeviceIdBox, RoomId, UserId,
};
pub use sled::Error;
use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, Transactional, Tree,
};
use tracing::trace;
use uuid::Uuid;

use super::{
    caches::SessionStore, BackupKeys, Changes, CryptoStore, CryptoStoreError, InboundGroupSession,
    PickleKey, ReadOnlyAccount, Result, RoomKeyCounts, Session,
};
use crate::{
    gossiping::{GossipRequest, SecretInfo},
    identities::{ReadOnlyDevice, ReadOnlyUserIdentities},
    olm::{OutboundGroupSession, PickledInboundGroupSession, PrivateCrossSigningIdentity},
};

/// This needs to be 32 bytes long since AES-GCM requires it, otherwise we will
/// panic once we try to pickle a Signing object.
const DEFAULT_PICKLE: &str = "DEFAULT_PICKLE_PASSPHRASE_123456";
const DATABASE_VERSION: u8 = 1;

trait EncodeKey {
    const SEPARATOR: u8 = 0xff;
    fn encode(&self) -> Vec<u8>;
}

impl EncodeKey for Uuid {
    fn encode(&self) -> Vec<u8> {
        self.as_u128().to_be_bytes().to_vec()
    }
}

impl EncodeKey for SecretName {
    fn encode(&self) -> Vec<u8> {
        [self.as_ref().as_bytes(), &[Self::SEPARATOR]].concat()
    }
}

impl EncodeKey for SecretInfo {
    fn encode(&self) -> Vec<u8> {
        match self {
            SecretInfo::KeyRequest(k) => k.encode(),
            SecretInfo::SecretRequest(s) => s.encode(),
        }
    }
}

impl EncodeKey for &RequestedKeyInfo {
    fn encode(&self) -> Vec<u8> {
        [
            self.room_id.as_bytes(),
            &[Self::SEPARATOR],
            self.sender_key.as_bytes(),
            &[Self::SEPARATOR],
            self.algorithm.as_ref().as_bytes(),
            &[Self::SEPARATOR],
            self.session_id.as_bytes(),
            &[Self::SEPARATOR],
        ]
        .concat()
    }
}

impl EncodeKey for &UserId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for &RoomId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for &str {
    fn encode(&self) -> Vec<u8> {
        [self.as_bytes(), &[Self::SEPARATOR]].concat()
    }
}

impl EncodeKey for (&str, &str) {
    fn encode(&self) -> Vec<u8> {
        [self.0.as_bytes(), &[Self::SEPARATOR], self.1.as_bytes(), &[Self::SEPARATOR]].concat()
    }
}

impl EncodeKey for (&str, &str, &str) {
    fn encode(&self) -> Vec<u8> {
        [
            self.0.as_bytes(),
            &[Self::SEPARATOR],
            self.1.as_bytes(),
            &[Self::SEPARATOR],
            self.2.as_bytes(),
            &[Self::SEPARATOR],
        ]
        .concat()
    }
}

pub fn decode_key_value(key: &[u8], position: usize) -> Option<String> {
    let values: Vec<&[u8]> = key.split(|v| *v == 0xff).collect();

    values.get(position).map(|s| String::from_utf8_lossy(s).to_string())
}

#[derive(Clone, Debug)]
pub struct AccountInfo {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    identity_keys: Arc<IdentityKeys>,
}

/// An in-memory only store that will forget all the E2EE key once it's dropped.
#[derive(Clone)]
pub struct SledStore {
    account_info: Arc<RwLock<Option<AccountInfo>>>,
    path: Option<PathBuf>,
    inner: Db,
    pickle_key: Arc<PickleKey>,

    session_cache: SessionStore,
    tracked_users_cache: Arc<DashSet<UserId>>,
    users_for_key_query_cache: Arc<DashSet<UserId>>,

    account: Tree,
    private_identity: Tree,
    misc: Tree,

    olm_hashes: Tree,
    sessions: Tree,
    inbound_group_sessions: Tree,
    outbound_group_sessions: Tree,

    outgoing_secret_requests: Tree,
    unsent_secret_requests: Tree,
    secret_requests_by_info: Tree,

    devices: Tree,
    identities: Tree,

    tracked_users: Tree,
}

impl std::fmt::Debug for SledStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(path) = &self.path {
            f.debug_struct("SledStore").field("path", &path).finish()
        } else {
            f.debug_struct("SledStore").field("path", &"memory store").finish()
        }
    }
}

impl From<TransactionError<serde_json::Error>> for CryptoStoreError {
    fn from(e: TransactionError<serde_json::Error>) -> Self {
        match e {
            TransactionError::Abort(e) => CryptoStoreError::Serialization(e),
            TransactionError::Storage(e) => CryptoStoreError::Database(e),
        }
    }
}

impl SledStore {
    /// Open the sled based cryptostore at the given path using the given
    /// passphrase to encrypt private data.
    pub fn open_with_passphrase(path: impl AsRef<Path>, passphrase: Option<&str>) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-crypto");
        let db = Config::new().temporary(false).path(&path).open()?;

        SledStore::open_helper(db, Some(path), passphrase)
    }

    /// Create a sled based cryptostore using the given sled database.
    /// The given passphrase will be used to encrypt private data.
    pub fn open_with_database(db: Db, passphrase: Option<&str>) -> Result<Self> {
        SledStore::open_helper(db, None, passphrase)
    }

    fn get_account_info(&self) -> Option<AccountInfo> {
        self.account_info.read().unwrap().clone()
    }

    async fn reset_backup_state(&self) -> Result<()> {
        let mut pickles: Vec<(String, PickledInboundGroupSession)> = self
            .inbound_group_sessions
            .iter()
            .map(|p| {
                let item = p?;
                Ok((
                    decode_key_value(&item.0, 0).expect("Inbound group session key is invalid"),
                    serde_json::from_slice(&item.1).map_err(CryptoStoreError::Serialization)?,
                ))
            })
            .collect::<Result<_>>()?;

        for (_, pickle) in &mut pickles {
            pickle.backed_up = false;
        }

        let ret: Result<(), TransactionError<serde_json::Error>> =
            self.inbound_group_sessions.transaction(|inbound_sessions| {
                for (session_id, pickle) in &pickles {
                    let key =
                        (pickle.room_id.as_str(), pickle.sender_key.as_str(), session_id.as_str())
                            .encode();
                    inbound_sessions.insert(
                        key.as_slice(),
                        serde_json::to_vec(&pickle).map_err(ConflictableTransactionError::Abort)?,
                    )?;
                }

                Ok(())
            });

        ret?;

        self.inner.flush_async().await?;

        Ok(())
    }

    fn upgrade(&self) -> Result<()> {
        let version = self
            .inner
            .get("store_version")?
            .map(|v| {
                let (version_bytes, _) = v.split_at(std::mem::size_of::<u8>());
                u8::from_be_bytes(version_bytes.try_into().unwrap_or_default())
            })
            .unwrap_or_default();

        if version != DATABASE_VERSION {
            trace!(
                version = version,
                new_version = DATABASE_VERSION,
                "Upgrading the Sled crypto store"
            );
        }

        if version == 0 {
            // We changed the schema but migrating this isn't important since we
            // rotate the group sessions relatively often anyways so we just
            // clear the tree.
            self.outbound_group_sessions.clear()?;
        }

        self.inner.insert("version", DATABASE_VERSION.to_be_bytes().as_ref())?;
        self.inner.flush()?;

        Ok(())
    }

    fn open_helper(db: Db, path: Option<PathBuf>, passphrase: Option<&str>) -> Result<Self> {
        let account = db.open_tree("account")?;
        let private_identity = db.open_tree("private_identity")?;

        let sessions = db.open_tree("session")?;
        let inbound_group_sessions = db.open_tree("inbound_group_sessions")?;

        let outbound_group_sessions = db.open_tree("outbound_group_sessions")?;

        let tracked_users = db.open_tree("tracked_users")?;
        let olm_hashes = db.open_tree("olm_hashes")?;

        let devices = db.open_tree("devices")?;
        let identities = db.open_tree("identities")?;

        let outgoing_secret_requests = db.open_tree("outgoing_secret_requests")?;
        let unsent_secret_requests = db.open_tree("unsent_secret_requests")?;
        let secret_requests_by_info = db.open_tree("secret_requests_by_info")?;
        let misc = db.open_tree("misc")?;

        let session_cache = SessionStore::new();

        let pickle_key = if let Some(passphrase) = passphrase {
            Self::get_or_create_pickle_key(passphrase, &db)?
        } else {
            PickleKey::try_from(DEFAULT_PICKLE.as_bytes().to_vec())
                .expect("Can't create default pickle key")
        };

        let database = Self {
            account_info: RwLock::new(None).into(),
            path,
            inner: db,
            pickle_key: pickle_key.into(),
            account,
            private_identity,
            sessions,
            misc,
            session_cache,
            tracked_users_cache: DashSet::new().into(),
            users_for_key_query_cache: DashSet::new().into(),
            inbound_group_sessions,
            outbound_group_sessions,
            outgoing_secret_requests,
            unsent_secret_requests,
            secret_requests_by_info,
            devices,
            tracked_users,
            olm_hashes,
            identities,
        };

        database.upgrade()?;

        Ok(database)
    }

    fn get_or_create_pickle_key(passphrase: &str, database: &Db) -> Result<PickleKey> {
        let key = if let Some(key) =
            database.get("pickle_key".encode())?.map(|v| serde_json::from_slice(&v))
        {
            PickleKey::from_encrypted(passphrase, key?)
                .map_err(|_| CryptoStoreError::UnpicklingError)?
        } else {
            let key = PickleKey::new();
            let encrypted = key.encrypt(passphrase);
            database.insert("pickle_key".encode(), serde_json::to_vec(&encrypted)?)?;
            key
        };

        Ok(key)
    }

    fn get_pickle_mode(&self) -> PicklingMode {
        self.pickle_key.pickle_mode()
    }

    fn get_pickle_key(&self) -> &[u8] {
        self.pickle_key.key()
    }

    async fn load_tracked_users(&self) -> Result<()> {
        for value in self.tracked_users.iter() {
            let (user, dirty) = value?;
            let user = UserId::try_from(String::from_utf8_lossy(&user).to_string())?;
            let dirty = dirty.get(0).map(|d| *d == 1).unwrap_or(true);

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

        self.outbound_group_sessions
            .get(room_id.encode())?
            .map(|p| serde_json::from_slice(&p).map_err(CryptoStoreError::Serialization))
            .transpose()?
            .map(|p| {
                OutboundGroupSession::from_pickle(
                    account_info.device_id,
                    account_info.identity_keys,
                    p,
                    self.get_pickle_mode(),
                )
                .map_err(CryptoStoreError::OlmGroupSession)
            })
            .transpose()
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        let account_pickle = if let Some(a) = changes.account {
            Some(a.pickle(self.get_pickle_mode()).await)
        } else {
            None
        };

        let private_identity_pickle = if let Some(i) = changes.private_identity {
            Some(i.pickle(self.get_pickle_key()).await?)
        } else {
            None
        };

        #[cfg(feature = "backups_v1")]
        let recovery_key_pickle = changes.recovery_key.map(|r| r.pickle(self.get_pickle_key()));

        let device_changes = changes.devices;
        let mut session_changes = HashMap::new();

        for session in changes.sessions {
            let sender_key = session.sender_key();
            let session_id = session.session_id();

            let pickle = session.pickle(self.get_pickle_mode()).await;
            let key = (sender_key, session_id).encode();

            self.session_cache.add(session).await;
            session_changes.insert(key, pickle);
        }

        let mut inbound_session_changes = HashMap::new();

        for session in changes.inbound_group_sessions {
            let room_id = session.room_id();
            let sender_key = session.sender_key();
            let session_id = session.session_id();
            let key = (room_id.as_str(), sender_key, session_id).encode();
            let pickle = session.pickle(self.get_pickle_mode()).await;

            inbound_session_changes.insert(key, pickle);
        }

        let mut outbound_session_changes = HashMap::new();

        for session in changes.outbound_group_sessions {
            let room_id = session.room_id();
            let pickle = session.pickle(self.get_pickle_mode()).await;

            outbound_session_changes.insert(room_id.clone(), pickle);
        }

        let identity_changes = changes.identities;
        let olm_hashes = changes.message_hashes;
        let key_requests = changes.key_requests;
        #[cfg(feature = "backups_v1")]
        let backup_version = changes.backup_version;

        let ret: Result<(), TransactionError<serde_json::Error>> = (
            &self.account,
            &self.private_identity,
            &self.devices,
            &self.identities,
            &self.sessions,
            &self.inbound_group_sessions,
            &self.outbound_group_sessions,
            &self.olm_hashes,
            &self.outgoing_secret_requests,
            &self.unsent_secret_requests,
            &self.secret_requests_by_info,
        )
            .transaction(
                |(
                    account,
                    private_identity,
                    devices,
                    identities,
                    sessions,
                    inbound_sessions,
                    outbound_sessions,
                    hashes,
                    outgoing_secret_requests,
                    unsent_secret_requests,
                    secret_requests_by_info,
                )| {
                    if let Some(a) = &account_pickle {
                        account.insert(
                            "account".encode(),
                            serde_json::to_vec(a).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    if let Some(i) = &private_identity_pickle {
                        private_identity.insert(
                            "identity".encode(),
                            serde_json::to_vec(&i).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    #[cfg(feature = "backups_v1")]
                    if let Some(r) = &recovery_key_pickle {
                        account.insert(
                            "recovery_key_v1".encode(),
                            serde_json::to_vec(r).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    #[cfg(feature = "backups_v1")]
                    if let Some(b) = &backup_version {
                        account.insert(
                            "backup_version_v1".encode(),
                            serde_json::to_vec(b).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for device in device_changes.new.iter().chain(&device_changes.changed) {
                        let key = (device.user_id().as_str(), device.device_id().as_str()).encode();
                        let device = serde_json::to_vec(&device)
                            .map_err(ConflictableTransactionError::Abort)?;
                        devices.insert(key, device)?;
                    }

                    for device in &device_changes.deleted {
                        let key = (device.user_id().as_str(), device.device_id().as_str()).encode();
                        devices.remove(key)?;
                    }

                    for identity in identity_changes.changed.iter().chain(&identity_changes.new) {
                        identities.insert(
                            identity.user_id().encode(),
                            serde_json::to_vec(&identity)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (key, session) in &session_changes {
                        sessions.insert(
                            key.as_slice(),
                            serde_json::to_vec(&session)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (key, session) in &inbound_session_changes {
                        inbound_sessions.insert(
                            key.as_slice(),
                            serde_json::to_vec(&session)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (key, session) in &outbound_session_changes {
                        outbound_sessions.insert(
                            key.encode(),
                            serde_json::to_vec(&session)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for hash in &olm_hashes {
                        hashes.insert(
                            serde_json::to_vec(&hash)
                                .map_err(ConflictableTransactionError::Abort)?,
                            &[0],
                        )?;
                    }

                    for key_request in &key_requests {
                        secret_requests_by_info.insert(
                            (&key_request.info).encode(),
                            key_request.request_id.encode(),
                        )?;

                        let key_request_id = key_request.request_id.encode();

                        if key_request.sent_out {
                            unsent_secret_requests.remove(key_request_id.clone())?;
                            outgoing_secret_requests.insert(
                                key_request_id,
                                serde_json::to_vec(&key_request)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        } else {
                            outgoing_secret_requests.remove(key_request_id.clone())?;
                            unsent_secret_requests.insert(
                                key_request_id,
                                serde_json::to_vec(&key_request)
                                    .map_err(ConflictableTransactionError::Abort)?,
                            )?;
                        }
                    }

                    Ok(())
                },
            );

        ret?;
        self.inner.flush_async().await?;

        Ok(())
    }

    async fn get_outgoing_key_request_helper(&self, id: &[u8]) -> Result<Option<GossipRequest>> {
        let request = self
            .outgoing_secret_requests
            .get(id)?
            .map(|r| serde_json::from_slice(&r))
            .transpose()?;

        let request = if request.is_none() {
            self.unsent_secret_requests.get(id)?.map(|r| serde_json::from_slice(&r)).transpose()?
        } else {
            request
        };

        Ok(request)
    }
}

#[async_trait]
impl CryptoStore for SledStore {
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        if let Some(pickle) = self.account.get("account".encode())? {
            let pickle = serde_json::from_slice(&pickle)?;

            self.load_tracked_users().await?;

            let account = ReadOnlyAccount::from_pickle(pickle, self.get_pickle_mode())?;

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
        let account_info = AccountInfo {
            user_id: account.user_id.clone(),
            device_id: account.device_id.clone(),
            identity_keys: account.identity_keys.clone(),
        };

        *self.account_info.write().unwrap() = Some(account_info);

        let changes = Changes { account: Some(account), ..Default::default() };

        self.save_changes(changes).await
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        if let Some(i) = self.private_identity.get("identity".encode())? {
            let pickle = serde_json::from_slice(&i)?;
            Ok(Some(
                PrivateCrossSigningIdentity::from_pickle(pickle, self.get_pickle_key())
                    .await
                    .map_err(|_| CryptoStoreError::UnpicklingError)?,
            ))
        } else {
            Ok(None)
        }
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        self.save_changes(changes).await
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;

        if self.session_cache.get(sender_key).is_none() {
            let sessions: Result<Vec<Session>> = self
                .sessions
                .scan_prefix(sender_key.encode())
                .map(|s| serde_json::from_slice(&s?.1).map_err(CryptoStoreError::Serialization))
                .map(|p| {
                    Session::from_pickle(
                        account_info.user_id.clone(),
                        account_info.device_id.clone(),
                        account_info.identity_keys.clone(),
                        p?,
                        self.get_pickle_mode(),
                    )
                    .map_err(CryptoStoreError::SessionUnpickling)
                })
                .collect();

            self.session_cache.set_for_sender(sender_key, sessions?);
        }

        Ok(self.session_cache.get(sender_key))
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let key = (room_id.as_str(), sender_key, session_id).encode();
        let pickle = self.inbound_group_sessions.get(&key)?.map(|p| serde_json::from_slice(&p));

        if let Some(pickle) = pickle {
            Ok(Some(InboundGroupSession::from_pickle(pickle?, self.get_pickle_mode())?))
        } else {
            Ok(None)
        }
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        let pickles: Result<Vec<PickledInboundGroupSession>> = self
            .inbound_group_sessions
            .iter()
            .map(|p| serde_json::from_slice(&p?.1).map_err(CryptoStoreError::Serialization))
            .collect();

        Ok(pickles?
            .into_iter()
            .filter_map(|p| InboundGroupSession::from_pickle(p, self.get_pickle_mode()).ok())
            .collect())
    }

    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        let pickles: Vec<PickledInboundGroupSession> = self
            .inbound_group_sessions
            .iter()
            .map(|p| {
                let item = p?;
                serde_json::from_slice(&item.1).map_err(CryptoStoreError::Serialization)
            })
            .collect::<Result<_>>()?;

        let total = pickles.len();
        let backed_up = pickles.into_iter().filter(|p| p.backed_up).count();

        Ok(RoomKeyCounts { total, backed_up })
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        let pickles: Vec<InboundGroupSession> = self
            .inbound_group_sessions
            .iter()
            .map(|p| {
                let item = p?;
                serde_json::from_slice(&item.1).map_err(CryptoStoreError::from)
            })
            .filter_map(|p: Result<PickledInboundGroupSession, CryptoStoreError>| match p {
                Ok(p) => {
                    if !p.backed_up {
                        Some(
                            InboundGroupSession::from_pickle(p, self.get_pickle_mode())
                                .map_err(CryptoStoreError::from),
                        )
                    } else {
                        None
                    }
                }

                Err(p) => Some(Err(p)),
            })
            .take(limit)
            .collect::<Result<_>>()?;

        Ok(pickles)
    }

    async fn reset_backup_state(&self) -> Result<()> {
        self.reset_backup_state().await
    }

    async fn get_outbound_group_sessions(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        self.load_outbound_group_session(room_id).await
    }

    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        self.tracked_users_cache.contains(user_id)
    }

    fn has_users_for_key_query(&self) -> bool {
        !self.users_for_key_query_cache.is_empty()
    }

    fn users_for_key_query(&self) -> HashSet<UserId> {
        self.users_for_key_query_cache.iter().map(|u| u.clone()).collect()
    }

    fn tracked_users(&self) -> HashSet<UserId> {
        self.tracked_users_cache.to_owned().iter().map(|u| u.clone()).collect()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        let already_added = self.tracked_users_cache.insert(user.clone());

        if dirty {
            self.users_for_key_query_cache.insert(user.clone());
        } else {
            self.users_for_key_query_cache.remove(user);
        }

        self.tracked_users.insert(user.as_str(), &[dirty as u8])?;

        Ok(already_added)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let key = (user_id.as_str(), device_id.as_str()).encode();
        Ok(self.devices.get(key)?.map(|d| serde_json::from_slice(&d)).transpose()?)
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<DeviceIdBox, ReadOnlyDevice>> {
        self.devices
            .scan_prefix(user_id.encode())
            .map(|d| serde_json::from_slice(&d?.1).map_err(CryptoStoreError::Serialization))
            .map(|d| {
                let d: ReadOnlyDevice = d?;
                Ok((d.device_id().to_owned(), d))
            })
            .collect()
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
        Ok(self
            .identities
            .get(user_id.encode())?
            .map(|i| serde_json::from_slice(&i))
            .transpose()?)
    }

    async fn is_message_known(&self, message_hash: &crate::olm::OlmMessageHash) -> Result<bool> {
        Ok(self.olm_hashes.contains_key(serde_json::to_vec(message_hash)?)?)
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: Uuid,
    ) -> Result<Option<GossipRequest>> {
        let request_id = request_id.encode();

        self.get_outgoing_key_request_helper(&request_id).await
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let id = self.secret_requests_by_info.get(key_info.encode())?;

        if let Some(id) = id {
            self.get_outgoing_key_request_helper(&id).await
        } else {
            Ok(None)
        }
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        let requests: Result<Vec<GossipRequest>> = self
            .unsent_secret_requests
            .iter()
            .map(|i| serde_json::from_slice(&i?.1).map_err(CryptoStoreError::from))
            .collect();

        requests
    }

    async fn delete_outgoing_secret_requests(&self, request_id: Uuid) -> Result<()> {
        let ret: Result<(), TransactionError<serde_json::Error>> = (
            &self.outgoing_secret_requests,
            &self.unsent_secret_requests,
            &self.secret_requests_by_info,
        )
            .transaction(
                |(outgoing_key_requests, unsent_key_requests, key_requests_by_info)| {
                    let sent_request: Option<GossipRequest> = outgoing_key_requests
                        .remove(request_id.encode())?
                        .map(|r| serde_json::from_slice(&r))
                        .transpose()
                        .map_err(ConflictableTransactionError::Abort)?;

                    let unsent_request: Option<GossipRequest> = unsent_key_requests
                        .remove(request_id.encode())?
                        .map(|r| serde_json::from_slice(&r))
                        .transpose()
                        .map_err(ConflictableTransactionError::Abort)?;

                    if let Some(request) = sent_request {
                        key_requests_by_info.remove((&request.info).encode())?;
                    }

                    if let Some(request) = unsent_request {
                        key_requests_by_info.remove((&request.info).encode())?;
                    }

                    Ok(())
                },
            );

        ret?;
        self.inner.flush_async().await?;

        Ok(())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        let version = self
            .account
            .get("backup_version_v1".encode())?
            .map(|v| serde_json::from_slice(&v))
            .transpose()?;

        Ok(BackupKeys {
            backup_version: version,
            // TODO fetch the key as well.
            recovery_key: None,
        })
    }
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;

    use matrix_sdk_common::uuid::Uuid;
    use matrix_sdk_test::async_test;
    use olm_rs::outbound_group_session::OlmOutboundGroupSession;
    use ruma::{
        encryption::SignedKey, events::room_key_request::RequestedKeyInfo, room_id, user_id,
        DeviceId, EventEncryptionAlgorithm, UserId,
    };
    use tempfile::tempdir;

    use super::{CryptoStore, GossipRequest, SledStore};
    use crate::{
        gossiping::SecretInfo,
        identities::{
            device::test::get_device,
            user::test::{get_other_identity, get_own_identity},
        },
        olm::{
            GroupSessionKey, InboundGroupSession, OlmMessageHash, PrivateCrossSigningIdentity,
            ReadOnlyAccount, Session,
        },
        store::{Changes, DeviceChanges, IdentityChanges},
    };

    fn alice_id() -> UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> Box<DeviceId> {
        "ALICEDEVICE".into()
    }

    fn bob_id() -> UserId {
        user_id!("@bob:example.org")
    }

    fn bob_device_id() -> Box<DeviceId> {
        "BOBDEVICE".into()
    }

    async fn get_store(passphrase: Option<&str>) -> (SledStore, tempfile::TempDir) {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();

        let store = SledStore::open_with_passphrase(tmpdir_path, passphrase)
            .expect("Can't create a passphrase protected store");

        (store, tmpdir)
    }

    async fn get_loaded_store() -> (ReadOnlyAccount, SledStore, tempfile::TempDir) {
        let (store, dir) = get_store(None).await;
        let account = get_account();
        store.save_account(account.clone()).await.expect("Can't save account");

        (account, store, dir)
    }

    fn get_account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(&alice_id(), &alice_device_id())
    }

    async fn get_account_and_session() -> (ReadOnlyAccount, Session) {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());

        bob.generate_one_time_keys_helper(1).await;
        let one_time_key =
            bob.one_time_keys().await.curve25519().iter().next().unwrap().1.to_owned();
        let one_time_key = SignedKey::new(one_time_key, BTreeMap::new());
        let sender_key = bob.identity_keys().curve25519().to_owned();
        let session =
            alice.create_outbound_session_helper(&sender_key, &one_time_key).await.unwrap();

        (alice, session)
    }

    #[async_test]
    async fn create_store() {
        let tmpdir = tempdir().unwrap();
        let tmpdir_path = tmpdir.path().to_str().unwrap();
        let _ = SledStore::open_with_passphrase(tmpdir_path, None).expect("Can't create store");
    }

    #[async_test]
    async fn save_account() {
        let (store, _dir) = get_store(None).await;
        assert!(store.load_account().await.unwrap().is_none());
        let account = get_account();

        store.save_account(account).await.expect("Can't save account");
    }

    #[async_test]
    async fn load_account() {
        let (store, _dir) = get_store(None).await;
        let account = get_account();

        store.save_account(account.clone()).await.expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[async_test]
    async fn load_account_with_passphrase() {
        let (store, _dir) = get_store(Some("secret_passphrase")).await;
        let account = get_account();

        store.save_account(account.clone()).await.expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
    }

    #[async_test]
    async fn save_and_share_account() {
        let (store, _dir) = get_store(None).await;
        let account = get_account();

        store.save_account(account.clone()).await.expect("Can't save account");

        account.mark_as_shared();
        account.update_uploaded_key_count(50);

        store.save_account(account.clone()).await.expect("Can't save account");

        let loaded_account = store.load_account().await.expect("Can't load account");
        let loaded_account = loaded_account.unwrap();

        assert_eq!(account, loaded_account);
        assert_eq!(account.uploaded_key_count(), loaded_account.uploaded_key_count());
    }

    #[async_test]
    async fn load_sessions() {
        let (store, _dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;
        store.save_account(account.clone()).await.expect("Can't save account");

        let changes = Changes { sessions: vec![session.clone()], ..Default::default() };

        store.save_changes(changes).await.unwrap();

        let sessions =
            store.get_sessions(&session.sender_key).await.expect("Can't load sessions").unwrap();
        let loaded_session = sessions.lock().await.get(0).cloned().unwrap();

        assert_eq!(&session, &loaded_session);
    }

    #[async_test]
    async fn add_and_save_session() {
        let (store, dir) = get_store(None).await;
        let (account, session) = get_account_and_session().await;
        let sender_key = session.sender_key.to_owned();
        let session_id = session.session_id().to_owned();

        store.save_account(account.clone()).await.expect("Can't save account");

        let changes = Changes { sessions: vec![session.clone()], ..Default::default() };
        store.save_changes(changes).await.unwrap();

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());

        drop(store);

        let store = SledStore::open_with_passphrase(dir.path(), None).expect("Can't create store");

        let loaded_account = store.load_account().await.unwrap().unwrap();
        assert_eq!(account, loaded_account);

        let sessions = store.get_sessions(&sender_key).await.unwrap().unwrap();
        let sessions_lock = sessions.lock().await;
        let session = &sessions_lock[0];

        assert_eq!(session_id, session.session_id());
    }

    #[async_test]
    async fn save_inbound_group_session() {
        let (account, store, _dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &room_id!("!test:localhost"),
            GroupSessionKey(outbound_session.session_key()),
            None,
        )
        .expect("Can't create session");

        let changes = Changes { inbound_group_sessions: vec![session], ..Default::default() };

        store.save_changes(changes).await.expect("Can't save group session");
    }

    #[async_test]
    async fn load_inbound_group_session() {
        let (account, store, dir) = get_loaded_store().await;

        let identity_keys = account.identity_keys();
        let outbound_session = OlmOutboundGroupSession::new();
        let session = InboundGroupSession::new(
            identity_keys.curve25519(),
            identity_keys.ed25519(),
            &room_id!("!test:localhost"),
            GroupSessionKey(outbound_session.session_key()),
            None,
        )
        .expect("Can't create session");

        let mut export = session.export().await;

        export.forwarding_curve25519_key_chain = vec!["some_chain".to_owned()];

        let session = InboundGroupSession::from_export(export).unwrap();

        let changes =
            Changes { inbound_group_sessions: vec![session.clone()], ..Default::default() };

        store.save_changes(changes).await.expect("Can't save group session");

        drop(store);

        let store = SledStore::open_with_passphrase(dir.path(), None).expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_session = store
            .get_inbound_group_session(&session.room_id, &session.sender_key, session.session_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(session, loaded_session);
        let export = loaded_session.export().await;
        assert!(!export.forwarding_curve25519_key_chain.is_empty())
    }

    #[async_test]
    async fn test_tracked_users() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        assert!(store.update_tracked_user(device.user_id(), false).await.unwrap());
        assert!(!store.update_tracked_user(device.user_id(), false).await.unwrap());

        assert!(store.is_user_tracked(device.user_id()));
        assert!(!store.users_for_key_query().contains(device.user_id()));
        assert!(!store.update_tracked_user(device.user_id(), true).await.unwrap());
        assert!(store.users_for_key_query().contains(device.user_id()));
        drop(store);

        let store = SledStore::open_with_passphrase(dir.path(), None).expect("Can't create store");

        store.load_account().await.unwrap();

        assert!(store.is_user_tracked(device.user_id()));
        assert!(store.users_for_key_query().contains(device.user_id()));

        store.update_tracked_user(device.user_id(), false).await.unwrap();
        assert!(!store.users_for_key_query().contains(device.user_id()));
        drop(store);

        let store = SledStore::open_with_passphrase(dir.path(), None).expect("Can't create store");

        store.load_account().await.unwrap();

        assert!(!store.users_for_key_query().contains(device.user_id()));
    }

    #[async_test]
    async fn device_saving() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        let changes = Changes {
            devices: DeviceChanges { changed: vec![device.clone()], ..Default::default() },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        drop(store);

        let store = SledStore::open_with_passphrase(dir.path(), None).expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device =
            store.get_device(device.user_id(), device.device_id()).await.unwrap().unwrap();

        assert_eq!(device, loaded_device);

        for algorithm in loaded_device.algorithms() {
            assert!(device.algorithms().contains(algorithm));
        }
        assert_eq!(device.algorithms().len(), loaded_device.algorithms().len());
        assert_eq!(device.keys(), loaded_device.keys());

        let user_devices = store.get_user_devices(device.user_id()).await.unwrap();
        assert_eq!(&**user_devices.keys().next().unwrap(), device.device_id());
        assert_eq!(user_devices.values().next().unwrap(), &device);
    }

    #[async_test]
    async fn device_deleting() {
        let (_account, store, dir) = get_loaded_store().await;
        let device = get_device();

        let changes = Changes {
            devices: DeviceChanges { changed: vec![device.clone()], ..Default::default() },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        let changes = Changes {
            devices: DeviceChanges { deleted: vec![device.clone()], ..Default::default() },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();
        drop(store);

        let store = SledStore::open_with_passphrase(dir.path(), None).expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_device = store.get_device(device.user_id(), device.device_id()).await.unwrap();

        assert!(loaded_device.is_none());
    }

    #[async_test]
    async fn user_saving() {
        let dir = tempdir().unwrap();
        let tmpdir_path = dir.path().to_str().unwrap();

        let user_id = user_id!("@example:localhost");
        let device_id: &DeviceId = "WSKKLTJZCL".into();

        let store = SledStore::open_with_passphrase(tmpdir_path, None).expect("Can't create store");

        let account = ReadOnlyAccount::new(&user_id, device_id);

        store.save_account(account.clone()).await.expect("Can't save account");

        let own_identity = get_own_identity();

        let changes = Changes {
            identities: IdentityChanges {
                changed: vec![own_identity.clone().into()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.expect("Can't save identity");

        drop(store);

        let store = SledStore::open_with_passphrase(dir.path(), None).expect("Can't create store");

        store.load_account().await.unwrap();

        let loaded_user = store.get_user_identity(own_identity.user_id()).await.unwrap().unwrap();

        assert_eq!(loaded_user.master_key(), own_identity.master_key());
        assert_eq!(loaded_user.self_signing_key(), own_identity.self_signing_key());
        assert_eq!(loaded_user, own_identity.clone().into());

        let other_identity = get_other_identity();

        let changes = Changes {
            identities: IdentityChanges {
                changed: vec![other_identity.clone().into()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();

        let loaded_user = store.get_user_identity(other_identity.user_id()).await.unwrap().unwrap();

        assert_eq!(loaded_user.master_key(), other_identity.master_key());
        assert_eq!(loaded_user.self_signing_key(), other_identity.self_signing_key());
        assert_eq!(loaded_user, other_identity.into());

        own_identity.mark_as_verified();

        let changes = Changes {
            identities: IdentityChanges {
                changed: vec![own_identity.into()],
                ..Default::default()
            },
            ..Default::default()
        };

        store.save_changes(changes).await.unwrap();
        let loaded_user = store.get_user_identity(&user_id).await.unwrap().unwrap();
        assert!(loaded_user.own().unwrap().is_verified())
    }

    #[async_test]
    async fn private_identity_saving() {
        let (_, store, _dir) = get_loaded_store().await;
        assert!(store.load_identity().await.unwrap().is_none());
        let identity = PrivateCrossSigningIdentity::new(alice_id()).await;

        let changes = Changes { private_identity: Some(identity.clone()), ..Default::default() };

        store.save_changes(changes).await.unwrap();
        let loaded_identity = store.load_identity().await.unwrap().unwrap();
        assert_eq!(identity.user_id(), loaded_identity.user_id());
    }

    #[async_test]
    async fn olm_hash_saving() {
        let (_, store, _dir) = get_loaded_store().await;

        let hash =
            OlmMessageHash { sender_key: "test_sender".to_owned(), hash: "test_hash".to_owned() };

        let mut changes = Changes::default();
        changes.message_hashes.push(hash.clone());

        assert!(!store.is_message_known(&hash).await.unwrap());
        store.save_changes(changes).await.unwrap();
        assert!(store.is_message_known(&hash).await.unwrap());
    }

    #[async_test]
    async fn key_request_saving() {
        let (account, store, _dir) = get_loaded_store().await;

        let id = Uuid::new_v4();
        let info: SecretInfo = RequestedKeyInfo::new(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id!("!test:localhost"),
            "test_sender_key".to_string(),
            "test_session_id".to_string(),
        )
        .into();

        let request = GossipRequest {
            request_recipient: account.user_id().to_owned(),
            request_id: id,
            info: info.clone(),
            sent_out: false,
        };

        assert!(store.get_outgoing_secret_requests(id).await.unwrap().is_none());

        let mut changes = Changes::default();
        changes.key_requests.push(request.clone());
        store.save_changes(changes).await.unwrap();

        let request = Some(request);

        let stored_request = store.get_outgoing_secret_requests(id).await.unwrap();
        assert_eq!(request, stored_request);

        let stored_request = store.get_secret_request_by_info(&info).await.unwrap();
        assert_eq!(request, stored_request);
        assert!(!store.get_unsent_secret_requests().await.unwrap().is_empty());

        let request = GossipRequest {
            request_recipient: account.user_id().to_owned(),
            request_id: id,
            info: info.clone(),
            sent_out: true,
        };

        let mut changes = Changes::default();
        changes.key_requests.push(request.clone());
        store.save_changes(changes).await.unwrap();

        assert!(store.get_unsent_secret_requests().await.unwrap().is_empty());
        let stored_request = store.get_outgoing_secret_requests(id).await.unwrap();
        assert_eq!(Some(request), stored_request);

        store.delete_outgoing_secret_requests(id).await.unwrap();

        let stored_request = store.get_outgoing_secret_requests(id).await.unwrap();
        assert_eq!(None, stored_request);

        let stored_request = store.get_secret_request_by_info(&info).await.unwrap();
        assert_eq!(None, stored_request);
        assert!(store.get_unsent_secret_requests().await.unwrap().is_empty());
    }
}
