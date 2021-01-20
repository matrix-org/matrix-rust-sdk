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
    convert::TryFrom,
    path::Path,
    sync::Arc,
};

use dashmap::DashSet;
use olm_rs::PicklingMode;
pub use sled::Error;
use sled::{
    transaction::{ConflictableTransactionError, TransactionError},
    Config, Db, Transactional, Tree,
};

use matrix_sdk_common::{
    async_trait,
    identifiers::{DeviceId, DeviceIdBox, RoomId, UserId},
    locks::Mutex,
};

use super::{
    caches::SessionStore, Changes, CryptoStore, CryptoStoreError, InboundGroupSession, PickleKey,
    ReadOnlyAccount, Result, Session,
};
use crate::{
    identities::{ReadOnlyDevice, UserIdentities},
    olm::{PickledInboundGroupSession, PrivateCrossSigningIdentity},
};

/// This needs to be 32 bytes long since AES-GCM requires it, otherwise we will
/// panic once we try to pickle a Signing object.
const DEFAULT_PICKLE: &str = "DEFAULT_PICKLE_PASSPHRASE_123456";

/// An in-memory only store that will forget all the E2EE key once it's dropped.
#[derive(Debug, Clone)]
pub struct SledStore {
    inner: Db,
    pickle_key: Arc<PickleKey>,

    session_cache: SessionStore,
    tracked_users_cache: Arc<DashSet<UserId>>,
    users_for_key_query_cache: Arc<DashSet<UserId>>,

    account: Tree,
    private_identity: Tree,

    olm_hashes: Tree,
    sessions: Tree,
    inbound_group_sessions: Tree,

    devices: Tree,
    identities: Tree,

    tracked_users: Tree,
    users_for_key_query: Tree,
    values: Tree,
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
    pub fn open_with_passphrase(path: impl AsRef<Path>, passphrase: Option<&str>) -> Result<Self> {
        let path = path.as_ref().join("matrix-sdk-crypto");
        let db = Config::new().temporary(false).path(path).open()?;

        SledStore::open_helper(db, passphrase)
    }

    fn open_helper(db: Db, passphrase: Option<&str>) -> Result<Self> {
        let account = db.open_tree("account")?;
        let private_identity = db.open_tree("private_identity")?;

        let sessions = db.open_tree("session")?;
        let inbound_group_sessions = db.open_tree("inbound_group_sessions")?;
        let tracked_users = db.open_tree("tracked_users")?;
        let users_for_key_query = db.open_tree("users_for_key_query")?;
        let olm_hashes = db.open_tree("olm_hashes")?;

        let devices = db.open_tree("devices")?;
        let identities = db.open_tree("identities")?;
        let values = db.open_tree("values")?;

        let session_cache = SessionStore::new();

        let pickle_key = if let Some(passphrase) = passphrase {
            Self::get_or_create_pickle_key(&passphrase, &db)?
        } else {
            PickleKey::try_from(DEFAULT_PICKLE.as_bytes().to_vec())
                .expect("Can't create default pickle key")
        };

        Ok(Self {
            inner: db,
            pickle_key: pickle_key.into(),
            account,
            private_identity,
            sessions,
            session_cache,
            tracked_users_cache: DashSet::new().into(),
            users_for_key_query_cache: DashSet::new().into(),
            inbound_group_sessions,
            devices,
            tracked_users,
            users_for_key_query,
            olm_hashes,
            identities,
            values,
        })
    }

    fn get_or_create_pickle_key(passphrase: &str, database: &Db) -> Result<PickleKey> {
        let key = if let Some(key) = database
            .get("pickle_key")?
            .map(|v| serde_json::from_slice(&v))
        {
            PickleKey::from_encrypted(passphrase, key?)
                .map_err(|_| CryptoStoreError::UnpicklingError)?
        } else {
            let key = PickleKey::new();
            let encrypted = key.encrypt(passphrase);
            database.insert("pickle_key", serde_json::to_vec(&encrypted)?)?;
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

    pub async fn save_changes(&self, changes: Changes) -> Result<()> {
        let account_pickle = if let Some(a) = changes.account {
            Some(a.pickle(self.get_pickle_mode()).await)
        } else {
            None
        };

        let private_identity_pickle = if let Some(i) = changes.private_identity {
            Some(i.pickle(DEFAULT_PICKLE.as_bytes()).await?)
        } else {
            None
        };

        let device_changes = changes.devices;
        let mut session_changes = HashMap::new();

        for session in changes.sessions {
            let sender_key = session.sender_key();
            let session_id = session.session_id();

            let pickle = session.pickle(self.get_pickle_mode()).await;
            let key = format!("{}{}", sender_key, session_id);

            self.session_cache.add(session).await;
            session_changes.insert(key, pickle);
        }

        let mut inbound_session_changes = HashMap::new();

        for session in changes.inbound_group_sessions {
            let room_id = session.room_id();
            let sender_key = session.sender_key();
            let session_id = session.session_id();
            let key = format!("{}{}{}", room_id, sender_key, session_id);
            let pickle = session.pickle(self.get_pickle_mode()).await;

            inbound_session_changes.insert(key, pickle);
        }

        let identity_changes = changes.identities;
        let olm_hashes = changes.message_hashes;

        let ret: std::result::Result<(), TransactionError<serde_json::Error>> = (
            &self.account,
            &self.private_identity,
            &self.devices,
            &self.identities,
            &self.sessions,
            &self.inbound_group_sessions,
            &self.olm_hashes,
        )
            .transaction(
                |(
                    account,
                    private_identity,
                    devices,
                    identities,
                    sessions,
                    inbound_sessions,
                    hashes,
                )| {
                    if let Some(a) = &account_pickle {
                        account.insert(
                            "account",
                            serde_json::to_vec(a).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    if let Some(i) = &private_identity_pickle {
                        private_identity.insert(
                            "identity",
                            serde_json::to_vec(&i).map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for device in device_changes.new.iter().chain(&device_changes.changed) {
                        let key = format!("{}{}", device.user_id(), device.device_id());
                        let device = serde_json::to_vec(&device)
                            .map_err(ConflictableTransactionError::Abort)?;
                        devices.insert(key.as_str(), device)?;
                    }

                    for device in &device_changes.deleted {
                        let key = format!("{}{}", device.user_id(), device.device_id());
                        devices.remove(key.as_str())?;
                    }

                    for identity in identity_changes.changed.iter().chain(&identity_changes.new) {
                        identities.insert(
                            identity.user_id().as_str(),
                            serde_json::to_vec(&identity)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (key, session) in &session_changes {
                        sessions.insert(
                            key.as_str(),
                            serde_json::to_vec(&session)
                                .map_err(ConflictableTransactionError::Abort)?,
                        )?;
                    }

                    for (key, session) in &inbound_session_changes {
                        inbound_sessions.insert(
                            key.as_str(),
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

                    Ok(())
                },
            );

        ret?;
        self.inner.flush_async().await?;

        Ok(())
    }
}

#[async_trait]
impl CryptoStore for SledStore {
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        if let Some(pickle) = self.account.get("account")? {
            let pickle = serde_json::from_slice(&pickle)?;

            self.load_tracked_users().await?;

            Ok(Some(ReadOnlyAccount::from_pickle(
                pickle,
                self.get_pickle_mode(),
            )?))
        } else {
            Ok(None)
        }
    }

    async fn save_account(&self, account: ReadOnlyAccount) -> Result<()> {
        let pickle = account.pickle(self.get_pickle_mode()).await;
        self.account
            .insert("account", serde_json::to_vec(&pickle)?)?;

        Ok(())
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        self.save_changes(changes).await
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        let account = self
            .load_account()
            .await?
            .ok_or(CryptoStoreError::AccountUnset)?;

        if self.session_cache.get(sender_key).is_none() {
            let sessions: Result<Vec<Session>> = self
                .sessions
                .scan_prefix(sender_key)
                .map(|s| serde_json::from_slice(&s?.1).map_err(CryptoStoreError::Serialization))
                .map(|p| {
                    Session::from_pickle(
                        account.user_id.clone(),
                        account.device_id.clone(),
                        account.identity_keys.clone(),
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
        let key = format!("{}{}{}", room_id, sender_key, session_id);
        let pickle = self
            .inbound_group_sessions
            .get(&key)?
            .map(|p| serde_json::from_slice(&p));

        if let Some(pickle) = pickle {
            Ok(Some(InboundGroupSession::from_pickle(
                pickle?,
                self.get_pickle_mode(),
            )?))
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

    fn users_for_key_query(&self) -> HashSet<UserId> {
        #[allow(clippy::map_clone)]
        self.users_for_key_query_cache
            .iter()
            .map(|u| u.clone())
            .collect()
    }

    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        self.tracked_users_cache.contains(user_id)
    }

    fn has_users_for_key_query(&self) -> bool {
        !self.users_for_key_query_cache.is_empty()
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
        let key = format!("{}{}", user_id, device_id);

        if let Some(d) = self.devices.get(key)? {
            Ok(Some(serde_json::from_slice(&d)?))
        } else {
            Ok(None)
        }
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<DeviceIdBox, ReadOnlyDevice>> {
        self.devices
            .scan_prefix(user_id.as_str())
            .map(|d| serde_json::from_slice(&d?.1).map_err(CryptoStoreError::Serialization))
            .map(|d| {
                let d: ReadOnlyDevice = d?;
                Ok((d.device_id().to_owned(), d))
            })
            .collect()
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentities>> {
        Ok(self
            .identities
            .get(user_id.as_str())?
            .map(|i| serde_json::from_slice(&i))
            .transpose()?)
    }

    async fn save_value(&self, key: String, value: String) -> Result<()> {
        self.values.insert(key.as_str(), value.as_str())?;
        Ok(())
    }

    async fn remove_value(&self, key: &str) -> Result<()> {
        self.values.remove(key)?;
        Ok(())
    }

    async fn get_value(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .values
            .get(key)?
            .map(|v| String::from_utf8_lossy(&v).to_string()))
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        if let Some(i) = self.private_identity.get("identity")? {
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

    async fn is_message_known(&self, message_hash: &crate::olm::OlmMessageHash) -> Result<bool> {
        Ok(self
            .olm_hashes
            .contains_key(serde_json::to_vec(message_hash)?)?)
    }
}

#[cfg(test)]
mod test {}
