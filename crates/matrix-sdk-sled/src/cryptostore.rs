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
    collections::{BTreeMap, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use anyhow::anyhow;
use dashmap::DashSet;
use matrix_sdk_common::{
    async_trait,
    locks::Mutex,
    ruma::{
        encryption::DeviceKeys,
        events::{room_key_request::RequestedKeyInfo, secret::request::SecretName},
        DeviceId, DeviceKeyId, EventEncryptionAlgorithm, RoomId, TransactionId, UserId,
    },
};
use matrix_sdk_crypto::{
    olm::{
        InboundGroupSession, OutboundGroupSession, PickledInboundGroupSession,
        PrivateCrossSigningIdentity, Session,
    },
    store::{
        caches::SessionStore, BackupKeys, Changes, CryptoStore, CryptoStoreError, IdentityKeys,
        PickleKey, PicklingMode, RecoveryKey, Result, RoomKeyCounts,
    },
    GossipRequest, LocalTrust, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentities, SecretInfo,
};
use serde::{Deserialize, Serialize};
pub use sled::Error;
use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, IVec, Transactional, Tree,
};
use tracing::debug;

/// This needs to be 32 bytes long since AES-GCM requires it, otherwise we will
/// panic once we try to pickle a Signing object.
const DEFAULT_PICKLE: &str = "DEFAULT_PICKLE_PASSPHRASE_123456";
const DATABASE_VERSION: u8 = 3;

trait EncodeKey {
    const SEPARATOR: u8 = 0xff;
    fn encode(&self) -> Vec<u8>;
}

impl<T: EncodeKey> EncodeKey for &T {
    fn encode(&self) -> Vec<u8> {
        T::encode(self)
    }
}

impl<T: EncodeKey> EncodeKey for Box<T> {
    fn encode(&self) -> Vec<u8> {
        T::encode(self)
    }
}

impl EncodeKey for TransactionId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
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

impl EncodeKey for RequestedKeyInfo {
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

impl EncodeKey for UserId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for ReadOnlyDevice {
    fn encode(&self) -> Vec<u8> {
        (self.user_id().as_str(), self.device_id().as_str()).encode()
    }
}

impl EncodeKey for RoomId {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for String {
    fn encode(&self) -> Vec<u8> {
        self.as_str().encode()
    }
}

impl EncodeKey for str {
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

#[derive(Clone, Debug)]
pub struct AccountInfo {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    identity_keys: Arc<IdentityKeys>,
}

/// A [sled] based cryptostore.
///
/// [sled]: https://github.com/spacejam/sled#readme
#[derive(Clone)]
pub struct SledStore {
    account_info: Arc<RwLock<Option<AccountInfo>>>,
    path: Option<PathBuf>,
    inner: Db,
    pickle_key: Arc<PickleKey>,

    session_cache: SessionStore,
    tracked_users_cache: Arc<DashSet<Box<UserId>>>,
    users_for_key_query_cache: Arc<DashSet<Box<UserId>>>,

    account: Tree,
    private_identity: Tree,

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

impl SledStore {
    /// Open the sled based cryptostore at the given path using the given
    /// passphrase to encrypt private data.
    pub fn open_with_passphrase(
        path: impl AsRef<Path>,
        passphrase: Option<&str>,
    ) -> Result<Self, anyhow::Error> {
        let path = path.as_ref().join("matrix-sdk-crypto");
        let db = Config::new()
            .temporary(false)
            .path(&path)
            .open()
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;

        SledStore::open_helper(db, Some(path), passphrase)
    }

    /// Create a sled based cryptostore using the given sled database.
    /// The given passphrase will be used to encrypt private data.
    pub fn open_with_database(db: Db, passphrase: Option<&str>) -> Result<Self, anyhow::Error> {
        SledStore::open_helper(db, None, passphrase)
    }

    fn get_account_info(&self) -> Option<AccountInfo> {
        self.account_info.read().unwrap().clone()
    }

    async fn reset_backup_state(&self) -> Result<()> {
        let mut pickles: Vec<(IVec, PickledInboundGroupSession)> = self
            .inbound_group_sessions
            .iter()
            .map(|p| {
                let item = p.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
                Ok((
                    item.0,
                    serde_json::from_slice(&item.1).map_err(CryptoStoreError::Serialization)?,
                ))
            })
            .collect::<Result<_>>()?;

        for (_, pickle) in &mut pickles {
            pickle.backed_up = false;
        }

        let ret: Result<(), TransactionError<serde_json::Error>> =
            self.inbound_group_sessions.transaction(|inbound_sessions| {
                for (key, pickle) in &pickles {
                    inbound_sessions.insert(
                        key,
                        serde_json::to_vec(&pickle).map_err(ConflictableTransactionError::Abort)?,
                    )?;
                }

                Ok(())
            });

        ret.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;

        self.inner.flush_async().await.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;

        Ok(())
    }

    fn upgrade(&self) -> Result<()> {
        let version = self
            .inner
            .get("store_version")
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
            .map(|v| {
                let (version_bytes, _) = v.split_at(std::mem::size_of::<u8>());
                u8::from_be_bytes(version_bytes.try_into().unwrap_or_default())
            })
            .unwrap_or_default();

        if version != DATABASE_VERSION {
            debug!(version, new_version = DATABASE_VERSION, "Upgrading the Sled crypto store");
        }

        if version == 0 {
            // We changed the schema but migrating this isn't important since we
            // rotate the group sessions relatively often anyways so we just
            // clear the tree.
            self.outbound_group_sessions
                .clear()
                .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
        }

        if version <= 1 {
            #[derive(Serialize, Deserialize)]
            pub struct OldReadOnlyDevice {
                user_id: Box<UserId>,
                device_id: Box<DeviceId>,
                algorithms: Vec<EventEncryptionAlgorithm>,
                keys: BTreeMap<Box<DeviceKeyId>, String>,
                signatures: BTreeMap<Box<UserId>, BTreeMap<Box<DeviceKeyId>, String>>,
                display_name: Option<String>,
                deleted: bool,
                trust_state: LocalTrust,
            }

            #[allow(clippy::from_over_into)]
            impl Into<ReadOnlyDevice> for OldReadOnlyDevice {
                fn into(self) -> ReadOnlyDevice {
                    let mut device_keys = DeviceKeys::new(
                        self.user_id,
                        self.device_id,
                        self.algorithms,
                        self.keys,
                        self.signatures,
                    );
                    device_keys.unsigned.device_display_name = self.display_name;

                    ReadOnlyDevice::new(device_keys, self.trust_state)
                }
            }

            let devices: Vec<ReadOnlyDevice> = self
                .devices
                .iter()
                .map(|d| {
                    serde_json::from_slice(&d.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?.1)
                        .map_err(CryptoStoreError::Serialization)
                })
                .map(|d| {
                    let d: OldReadOnlyDevice = d?;
                    Ok(d.into())
                })
                .collect::<Result<Vec<ReadOnlyDevice>, CryptoStoreError>>()?;

            self.devices
                .transaction(move |tree| {
                    for device in &devices {
                        let key = device.encode();
                        let device = serde_json::to_vec(device)
                            .map_err(ConflictableTransactionError::Abort)?;
                        tree.insert(key, device)?;
                    }

                    Ok(())
                })
                .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
        }

        if version <= 2 {
            // We're treating our own device now differently, we're checking if
            // the keys match to what we have locally, remove the unchecked
            // device and mark our own user as dirty.
            if let Some(pickle) = self
                .account
                .get("account".encode())
                .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
            {
                let pickle = serde_json::from_slice(&pickle)?;
                let account = ReadOnlyAccount::from_pickle(pickle, self.get_pickle_mode())?;

                self.devices
                    .remove((account.user_id().as_str(), account.device_id.as_str()).encode())
                    .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
                self.tracked_users
                    .insert(account.user_id().as_str(), &[true as u8])
                    .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
            }
        }

        self.inner
            .insert("store_version", DATABASE_VERSION.to_be_bytes().as_ref())
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
        self.inner.flush().map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;

        Ok(())
    }

    fn open_helper(
        db: Db,
        path: Option<PathBuf>,
        passphrase: Option<&str>,
    ) -> Result<Self, anyhow::Error> {
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
        let key = if let Some(key) = database
            .get("pickle_key".encode())
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
            .map(|v| serde_json::from_slice(&v))
        {
            PickleKey::from_encrypted(passphrase, key?)
                .map_err(|_| CryptoStoreError::UnpicklingError)?
        } else {
            let key = PickleKey::new();
            let encrypted = key.encrypt(passphrase);
            database
                .insert("pickle_key".encode(), serde_json::to_vec(&encrypted)?)
                .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
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
        for value in &self.tracked_users {
            let (user, dirty) = value.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
            let user = UserId::parse(String::from_utf8_lossy(&user).to_string())?;
            let dirty = dirty.get(0).map(|d| *d == 1).unwrap_or(true);

            self.tracked_users_cache.insert(user.to_owned());

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
            .get(room_id.encode())
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
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

            outbound_session_changes.insert(room_id.to_owned(), pickle);
        }

        let identity_changes = changes.identities;
        let olm_hashes = changes.message_hashes;
        let key_requests = changes.key_requests;
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

                    if let Some(r) = &recovery_key_pickle {
                        account.insert(
                            "recovery_key_v1".encode(),
                            serde_json::to_vec(r).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    if let Some(b) = &backup_version {
                        account.insert(
                            "backup_version_v1".encode(),
                            serde_json::to_vec(b).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for device in device_changes.new.iter().chain(&device_changes.changed) {
                        let key = device.encode();
                        let device = serde_json::to_vec(&device)
                            .map_err(ConflictableTransactionError::Abort)?;
                        devices.insert(key, device)?;
                    }

                    for device in &device_changes.deleted {
                        let key = device.encode();
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
                        secret_requests_by_info
                            .insert(key_request.info.encode(), key_request.request_id.encode())?;

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

        ret.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
        self.inner.flush_async().await.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;

        Ok(())
    }

    async fn get_outgoing_key_request_helper(&self, id: &[u8]) -> Result<Option<GossipRequest>> {
        let request = self
            .outgoing_secret_requests
            .get(id)
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
            .map(|r| serde_json::from_slice(&r))
            .transpose()?;

        let request = if request.is_none() {
            self.unsent_secret_requests
                .get(id)
                .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
                .map(|r| serde_json::from_slice(&r))
                .transpose()?
        } else {
            request
        };

        Ok(request)
    }
}

#[async_trait]
impl CryptoStore for SledStore {
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        if let Some(pickle) = self
            .account
            .get("account".encode())
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
        {
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
        if let Some(i) = self
            .private_identity
            .get("identity".encode())
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
        {
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
                .map(|s| {
                    serde_json::from_slice(&s.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?.1)
                        .map_err(CryptoStoreError::Serialization)
                })
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
        let pickle = self
            .inbound_group_sessions
            .get(&key)
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
            .map(|p| serde_json::from_slice(&p));

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
            .map(|p| {
                serde_json::from_slice(&p.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?.1)
                    .map_err(CryptoStoreError::Serialization)
            })
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
                let item = p.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
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
                let item = p.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
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

    fn users_for_key_query(&self) -> HashSet<Box<UserId>> {
        self.users_for_key_query_cache.iter().map(|u| u.clone()).collect()
    }

    fn tracked_users(&self) -> HashSet<Box<UserId>> {
        self.tracked_users_cache.to_owned().iter().map(|u| u.clone()).collect()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        let already_added = self.tracked_users_cache.insert(user.to_owned());

        if dirty {
            self.users_for_key_query_cache.insert(user.to_owned());
        } else {
            self.users_for_key_query_cache.remove(user);
        }

        self.tracked_users
            .insert(user.as_str(), &[dirty as u8])
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;

        Ok(already_added)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let key = (user_id.as_str(), device_id.as_str()).encode();
        Ok(self
            .devices
            .get(key)
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
            .map(|d| serde_json::from_slice(&d))
            .transpose()?)
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<Box<DeviceId>, ReadOnlyDevice>> {
        self.devices
            .scan_prefix(user_id.encode())
            .map(|d| {
                serde_json::from_slice(&d.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?.1)
                    .map_err(CryptoStoreError::Serialization)
            })
            .map(|d| {
                let d: ReadOnlyDevice = d?;
                Ok((d.device_id().to_owned(), d))
            })
            .collect()
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
        Ok(self
            .identities
            .get(user_id.encode())
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
            .map(|i| serde_json::from_slice(&i))
            .transpose()?)
    }

    async fn is_message_known(
        &self,
        message_hash: &matrix_sdk_crypto::olm::OlmMessageHash,
    ) -> Result<bool> {
        Ok(self
            .olm_hashes
            .contains_key(serde_json::to_vec(message_hash)?)
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?)
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>> {
        let request_id = request_id.encode();

        self.get_outgoing_key_request_helper(&request_id).await
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let id = self
            .secret_requests_by_info
            .get(key_info.encode())
            .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;

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
            .map(|i| {
                serde_json::from_slice(&i.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?.1)
                    .map_err(CryptoStoreError::from)
            })
            .collect();

        requests
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
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
                        key_requests_by_info.remove(request.info.encode())?;
                    }

                    if let Some(request) = unsent_request {
                        key_requests_by_info.remove(request.info.encode())?;
                    }

                    Ok(())
                },
            );

        ret.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;
        self.inner.flush_async().await.map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?;

        Ok(())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        let key = {
            let backup_version = self
                .account
                .get("backup_version_v1".encode())
                .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
                .map(|v| serde_json::from_slice(&v))
                .transpose()?;

            let recovery_key = {
                self.account
                    .get("recovery_key_v1".encode())
                    .map_err(|e| CryptoStoreError::Backend(anyhow!(e)))?
                    .map(|p| serde_json::from_slice(&p))
                    .transpose()?
                    .map(|p| {
                        RecoveryKey::from_pickle(p, self.get_pickle_key())
                            .map_err(|_| CryptoStoreError::UnpicklingError)
                    })
                    .transpose()?
            };

            BackupKeys { backup_version, recovery_key }
        };

        Ok(key)
    }
}

#[cfg(test)]
mod test {
    use lazy_static::lazy_static;
    use matrix_sdk_crypto::cryptostore_integration_tests;
    use tempfile::{tempdir, TempDir};

    use super::SledStore;
    lazy_static! {
        /// This is an example for using doc comment attributes
        static ref TMP_DIR: TempDir = tempdir().unwrap();
    }

    async fn get_store(name: String, passphrase: Option<&str>) -> SledStore {
        let tmpdir_path = TMP_DIR.path().join(name);

        let store = SledStore::open_with_passphrase(tmpdir_path.to_str().unwrap(), passphrase)
            .expect("Can't create a passphrase protected store");

        store
    }

    cryptostore_integration_tests! { integration }
}
