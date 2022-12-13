// Copyright 2022 The Matrix.org Foundation C.I.C.
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
    any::Any,
    collections::{BTreeMap, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use dashmap::DashSet;
use futures_util::FutureExt;
use matrix_sdk_common::locks::Mutex;
use matrix_sdk_crypto::{
    olm::{
        IdentityKeys, InboundGroupSession, OutboundGroupSession, PickledInboundGroupSession,
        PickledOutboundGroupSession, PickledSession, PrivateCrossSigningIdentity, Session,
    },
    store::{
        caches::SessionStore, BackupKeys, Changes, CryptoStore, CryptoStoreError, RecoveryKey,
        Result, RoomKeyCounts,
    },
    types::{events::room_key_request::SupportedKeyInfo, EventEncryptionAlgorithm},
    GossipRequest, LocalTrust, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentities, SecretInfo,
};
use matrix_sdk_store_encryption::StoreCipher;
use redis::{
    aio::Connection, AsyncCommands, Client, ConnectionInfo, FromRedisValue, RedisConnectionInfo,
    RedisError, RedisFuture, RedisResult, ToRedisArgs,
};
use ruma::{
    events::secret::request::SecretName, DeviceId, OwnedDeviceId, OwnedUserId, RoomId,
    TransactionId, UserId,
};
use serde::{Deserialize, Serialize};

use crate::redis_shim::{RedisClientShim, RedisConnectionShim, RedisErrorShim};

/// This needs to be 32 bytes long since AES-GCM requires it, otherwise we will
/// panic once we try to pickle a Signing object.
const DEFAULT_PICKLE: &str = "DEFAULT_PICKLE_PASSPHRASE_123456";

trait RedisKey {
    fn redis_key(&self) -> String;
}

impl RedisKey for TransactionId {
    fn redis_key(&self) -> String {
        self.to_string()
    }
}

impl RedisKey for SecretName {
    fn redis_key(&self) -> String {
        self.to_string()
    }
}

impl RedisKey for SecretInfo {
    fn redis_key(&self) -> String {
        match self {
            SecretInfo::KeyRequest(k) => k.redis_key(),
            SecretInfo::SecretRequest(s) => s.redis_key(),
        }
    }
}

impl RedisKey for SupportedKeyInfo {
    fn redis_key(&self) -> String {
        (self.room_id(), self.algorithm(), self.session_id()).redis_key()
    }
}

impl RedisKey for EventEncryptionAlgorithm {
    fn redis_key(&self) -> String {
        self.as_ref().redis_key()
    }
}

impl RedisKey for &RoomId {
    fn redis_key(&self) -> String {
        self.as_str().redis_key()
    }
}

impl RedisKey for &str {
    fn redis_key(&self) -> String {
        String::from(*self)
    }
}

impl<A, B, C> RedisKey for (A, B, C)
where
    A: RedisKey,
    B: RedisKey,
    C: RedisKey,
{
    fn redis_key(&self) -> String {
        let mut ret = String::from("");
        ret += &self.0.redis_key();
        ret.push('|');
        ret += &self.1.redis_key();
        ret.push('|');
        ret += &self.2.redis_key();
        ret
    }
}

#[derive(Clone, Debug)]
pub struct AccountInfo {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    identity_keys: Arc<IdentityKeys>,
}

#[derive(Debug, Serialize, Deserialize)]
struct TrackedUser {
    user_id: OwnedUserId,
    dirty: bool,
}

/// A store that holds its information in a Redis database
#[derive(Clone)]
pub struct RedisStore<C>
where
    C: RedisClientShim,
{
    key_prefix: String,
    client: C,
    account_info: Arc<RwLock<Option<AccountInfo>>>,
    store_cipher: Option<Arc<StoreCipher>>,

    session_cache: SessionStore,
    tracked_users_cache: Arc<DashSet<OwnedUserId>>,
    users_for_key_query_cache: Arc<DashSet<OwnedUserId>>,
}

impl<C> std::fmt::Debug for RedisStore<C>
where
    C: RedisClientShim,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStore<C>")
            .field("client.get_connection_info().redis", &self.client.get_connection_info().redis)
            .field("key_prefix", &self.key_prefix)
            .field("account_info", &self.account_info)
            .finish()
    }
}

impl<C> RedisStore<C>
where
    C: RedisClientShim,
{
    #[allow(dead_code)]
    /// Open the Redis-based cryptostore at the given URL using the given
    /// passphrase to encrypt private data.
    pub async fn open_with_passphrase(client: C, passphrase: Option<&str>) -> Result<Self> {
        Self::open(client, passphrase, String::from("matrix-sdk-crypto|")).await
    }

    /// Open the Redis-based cryptostore at the given URL using the given
    /// passphrase to encrypt private data and assuming all Redis keys are
    /// prefixed with the given string.
    pub async fn open(client: C, passphrase: Option<&str>, key_prefix: String) -> Result<Self> {
        let mut connection = client.get_async_connection().await.unwrap();

        let store_cipher = if let Some(passphrase) = passphrase {
            Some(
                Self::get_or_create_store_cipher(passphrase, &key_prefix, &mut connection)
                    .await?
                    .into(),
            )
        } else {
            None
        };

        Ok(Self {
            key_prefix,
            client,
            account_info: RwLock::new(None).into(),
            store_cipher,
            session_cache: SessionStore::new(),
            tracked_users_cache: Arc::new(DashSet::new()),
            users_for_key_query_cache: Arc::new(DashSet::new()),
        })
    }

    fn get_account_info(&self) -> Option<AccountInfo> {
        self.account_info.read().unwrap().clone()
    }

    fn serialize_value(&self, event: &impl Serialize) -> Result<Vec<u8>, CryptoStoreError> {
        if let Some(key) = &self.store_cipher {
            key.encrypt_value(event).map_err(|e| CryptoStoreError::Backend(Box::new(e)))
        } else {
            Ok(serde_json::to_vec(event)?)
        }
    }

    fn deserialize_value<T: for<'b> Deserialize<'b>>(
        &self,
        event: &[u8],
    ) -> Result<T, CryptoStoreError> {
        if let Some(key) = &self.store_cipher {
            key.decrypt_value(event).map_err(|e| CryptoStoreError::Backend(Box::new(e)))
        } else {
            Ok(serde_json::from_slice(event)?)
        }
    }

    async fn reset_backup_state(&self) -> Result<()> {
        let redis_key = format!("{}inbound_group_sessions", self.key_prefix);
        let mut connection = self.client.get_async_connection().await?;

        // Read out all the sessions, set them as not backed up
        let sessions: Vec<(String, String)> = connection.hgetall(&redis_key).await?;
        let pickles: Vec<(String, PickledInboundGroupSession)> = sessions
            .into_iter()
            .map(|(k, s)| {
                let mut pickle: PickledInboundGroupSession =
                    self.deserialize_value(s.as_bytes()).unwrap();
                pickle.backed_up = false;
                (k, pickle)
            })
            .collect();

        // Write them back out in a transaction
        let mut pipeline = self.client.create_pipe();

        for (k, pickle) in pickles {
            pipeline.hset(&redis_key, &k, self.serialize_value(&pickle)?);
        }

        pipeline.query_async(&mut connection).await.unwrap();

        Ok(())
    }

    async fn get_or_create_store_cipher<Conn>(
        passphrase: &str,
        key_prefix: &str,
        connection: &mut Conn,
    ) -> Result<StoreCipher>
    where
        Conn: RedisConnectionShim,
    {
        let key_id = format!("{}{}", key_prefix, "store_cipher");
        let key_db_entry: Option<Vec<u8>> = connection.get(&key_id).await?;
        let key = if let Some(key_db_entry) = key_db_entry {
            StoreCipher::import(passphrase, &key_db_entry)
                .map_err(|_| CryptoStoreError::UnpicklingError)?
        } else {
            let cipher = StoreCipher::new().map_err(|e| CryptoStoreError::Backend(Box::new(e)))?;

            #[cfg(not(test))]
            let export = cipher.export(passphrase);
            #[cfg(test)]
            let export = cipher._insecure_export_fast_for_testing(passphrase);

            let export = export.map_err(|e| CryptoStoreError::Backend(Box::new(e)))?;
            connection.set(&key_id, export).await?;
            cipher
        };

        Ok(key)
    }

    async fn load_tracked_users(&self) -> Result<()> {
        let mut connection = self.client.get_async_connection().await?;
        let tracked_users: HashMap<String, Vec<u8>> =
            connection.hgetall(&format!("{}tracked_users", self.key_prefix)).await?;

        for (_, user) in tracked_users {
            let user: TrackedUser = self.deserialize_value(&user)?;

            self.tracked_users_cache.insert(user.user_id.to_owned());

            if user.dirty {
                self.users_for_key_query_cache.insert(user.user_id);
            }
        }

        Ok(())
    }

    async fn load_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        let account_info = self.get_account_info().ok_or(CryptoStoreError::AccountUnset)?;

        let mut connection = self.client.get_async_connection().await?;

        let redis_key = format!("{}outbound_session_changes", self.key_prefix);
        let session_vec: Option<Vec<u8>> = connection.hget(&redis_key, room_id.as_str()).await?;

        if let Some(session_vec) = session_vec {
            let session: PickledOutboundGroupSession = self.deserialize_value(&session_vec)?;

            let unpickled: OutboundGroupSession = OutboundGroupSession::from_pickle(
                account_info.device_id,
                account_info.identity_keys,
                session,
            )
            .map_err(CryptoStoreError::Pickle)?;

            Ok(Some(unpickled))
        } else {
            Ok(None)
        }
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
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

        let device_changes = changes.devices;
        let mut session_changes: HashMap<String, Vec<PickledSession>> = HashMap::new();

        for session in changes.sessions {
            let pickle = session.pickle().await;
            let sender_key = session.sender_key().to_base64();
            session_changes.entry(sender_key).or_default().push(pickle);

            self.session_cache.add(session).await;
        }

        let mut inbound_session_changes = HashMap::new();

        for session in changes.inbound_group_sessions {
            let room_id = session.room_id();
            let sender_key = session.sender_key();
            let session_id = session.session_id();
            let key = format!("{}|{}", room_id.as_str(), session_id);
            let pickle = session.pickle().await;

            inbound_session_changes.insert(key, pickle);
        }

        let mut outbound_session_changes = HashMap::new();

        for session in changes.outbound_group_sessions {
            let room_id = session.room_id().to_owned();
            let pickle = session.pickle().await;
            outbound_session_changes.insert(room_id.clone(), pickle);
        }

        let identity_changes = changes.identities;
        let olm_hashes = changes.message_hashes;
        let key_requests = changes.key_requests;
        let backup_version = changes.backup_version;

        let mut connection = self.client.get_async_connection().await?;

        // Wrap in a Redis transaction
        let mut pipeline = self.client.create_pipe();

        if let Some(a) = &account_pickle {
            pipeline.set_vec(&format!("{}account", self.key_prefix), self.serialize_value(a)?);
        }

        if let Some(i) = &private_identity_pickle {
            let redis_key = format!("{}private_identity", self.key_prefix);
            pipeline.set_vec(&redis_key, self.serialize_value(&i)?);
        }

        for (key, sessions) in &session_changes {
            let redis_key = format!("{}sessions|{}", self.key_prefix, key);
            pipeline.set_vec(&redis_key, self.serialize_value(sessions)?);
        }

        let redis_key = format!("{}inbound_group_sessions", self.key_prefix);
        for (key, inbound_group_sessions) in &inbound_session_changes {
            pipeline.hset(&redis_key, key, self.serialize_value(&inbound_group_sessions)?);
        }

        let redis_key = format!("{}outbound_session_changes", self.key_prefix);
        for (key, outbound_group_sessions) in &outbound_session_changes {
            pipeline.hset(&redis_key, key.as_str(), self.serialize_value(outbound_group_sessions)?);
        }

        let redis_key = format!("{}olm_hashes", self.key_prefix);
        for hash in &olm_hashes {
            pipeline.sadd(&redis_key, serde_json::to_string(hash)?);
        }

        let unsent_secret_requests_key = format!("{}unsent_secret_requests", self.key_prefix);

        for key_request in &key_requests {
            let key_request_id = key_request.request_id.redis_key();

            let secret_requests_by_info_key = format!(
                "{}secret_requests_by_info|{}",
                self.key_prefix,
                key_request.info.redis_key()
            );
            pipeline.set(&secret_requests_by_info_key, key_request.request_id.redis_key());

            let outgoing_secret_requests_key =
                format!("{}outgoing_secret_requests|{}", self.key_prefix, key_request_id);
            if key_request.sent_out {
                pipeline.hdel(&unsent_secret_requests_key, &key_request_id);
                pipeline
                    .set_vec(&outgoing_secret_requests_key, self.serialize_value(&key_request)?);
            } else {
                pipeline.del(&outgoing_secret_requests_key);
                pipeline.hset(
                    &unsent_secret_requests_key,
                    &key_request_id,
                    self.serialize_value(&key_request)?,
                );
            }
        }

        for device in device_changes.new.iter().chain(&device_changes.changed) {
            let redis_key = format!("{}devices|{}", self.key_prefix, device.user_id());

            pipeline.hset(&redis_key, device.device_id().as_str(), self.serialize_value(device)?);
        }

        for device in device_changes.deleted {
            let redis_key = format!("{}devices|{}", self.key_prefix, device.user_id());
            pipeline.hdel(&redis_key, device.device_id().as_str());
        }

        for identity in identity_changes.changed.iter().chain(&identity_changes.new) {
            let redis_key = format!("{}identities|{}", self.key_prefix, identity.user_id());

            pipeline.set_vec(&redis_key, self.serialize_value(identity)?);
        }

        if let Some(r) = &recovery_key_pickle {
            let redis_key = format!("{}recovery_key_v1", self.key_prefix);
            pipeline.set_vec(&redis_key, self.serialize_value(r)?);
        }

        if let Some(r) = &backup_version {
            let redis_key = format!("{}backup_version_v1", self.key_prefix);
            pipeline.set_vec(&redis_key, self.serialize_value(r)?);
        }

        pipeline.query_async(&mut connection).await?;

        Ok(())
    }

    async fn get_outgoing_key_request_helper(
        &self,
        request_id: &str,
    ) -> Result<Option<GossipRequest>> {
        let mut connection = self.client.get_async_connection().await?;
        let redis_key = format!("{}outgoing_secret_requests|{}", self.key_prefix, request_id);
        let req_vec: Option<Vec<u8>> = connection.get(&redis_key).await?;
        let request = req_vec.map(|req_vec| self.deserialize_value(&req_vec)).transpose()?;
        let request = if request.is_none() {
            let redis_key = format!("{}unsent_secret_requests", self.key_prefix);
            let req_bytes: Option<Vec<u8>> = connection.hget(&redis_key, request_id).await?;
            req_bytes.map(|req_bytes| self.deserialize_value(&req_bytes)).transpose()?
        } else {
            request
        };

        Ok(request)
    }

    /// Save a batch of tracked users.
    ///
    /// # Arguments
    ///
    /// * `tracked_users` - A list of tuples. The first element of the tuple is
    /// the user ID, the second element is if the user should be considered to
    /// be dirty.
    pub async fn save_tracked_users(
        &self,
        tracked_users: &[(&UserId, bool)],
    ) -> Result<(), CryptoStoreError> {
        let mut connection = self.client.get_async_connection().await.unwrap();

        let users: Vec<TrackedUser> = tracked_users
            .iter()
            .map(|(u, d)| TrackedUser { user_id: (*u).into(), dirty: *d })
            .collect();

        let mut pipeline = self.client.create_pipe();

        for user in users {
            pipeline.hset(
                &format!("{}tracked_users", self.key_prefix),
                user.user_id.as_str(),
                self.serialize_value(&user)?,
            );
        }

        pipeline.query_async(&mut connection).await?;

        Ok(())
    }
}

#[async_trait]
impl<C> CryptoStore for RedisStore<C>
where
    C: RedisClientShim,
{
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        let mut connection = self.client.get_async_connection().await?;
        let acct_json: Option<Vec<u8>> =
            connection.get(&format!("{}account", self.key_prefix)).await?;

        if let Some(pickle) = acct_json {
            let pickle = self.deserialize_value(&pickle)?;
            self.load_tracked_users().await?;

            let account = ReadOnlyAccount::from_pickle(pickle)?;

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
        self.save_changes(Changes { account: Some(account), ..Default::default() }).await
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        let mut connection = self.client.get_async_connection().await?;
        let key_prefix: String = format!("{}private_identity", self.key_prefix);
        let i_string: Option<Vec<u8>> = connection.get(&key_prefix).await?;
        if let Some(i) = i_string {
            let pickle = self.deserialize_value(&i)?;
            Ok(Some(
                PrivateCrossSigningIdentity::from_pickle(pickle)
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
            let mut connection = self.client.get_async_connection().await.unwrap();

            let key = format!("{}sessions|{}", self.key_prefix, sender_key);
            let sessions_list_as_vec: Option<Vec<u8>> = connection.get(&key).await?;
            let sessions_list: Vec<PickledSession> = match sessions_list_as_vec {
                Some(sessions_list_as_vec) => self.deserialize_value(&sessions_list_as_vec)?,
                _ => Vec::new(),
            };

            let sessions: Vec<Session> = sessions_list
                .into_iter()
                .map(|p| {
                    Session::from_pickle(
                        account_info.user_id.clone(),
                        account_info.device_id.clone(),
                        account_info.identity_keys.clone(),
                        p,
                    )
                })
                .collect();

            self.session_cache.set_for_sender(sender_key, sessions);
        }

        Ok(self.session_cache.get(sender_key))
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let key = format!("{room_id}|{session_id}");
        let redis_key = format!("{}inbound_group_sessions", self.key_prefix);
        let mut connection = self.client.get_async_connection().await?;
        let pickle_str: Option<String> = connection.hget(&redis_key, &key).await?;

        match pickle_str {
            Some(pickle_str) => Ok(Some(InboundGroupSession::from_pickle(
                self.deserialize_value(pickle_str.as_bytes())?,
            )?)),
            _ => Ok(None),
        }
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        let redis_key = format!("{}inbound_group_sessions", self.key_prefix);
        let mut connection = self.client.get_async_connection().await?;
        let igss: Vec<String> = connection.hvals(&redis_key).await?;

        let pickles: Result<Vec<PickledInboundGroupSession>> =
            igss.iter().map(|p| self.deserialize_value(p.as_bytes())).collect();

        Ok(pickles?.into_iter().filter_map(|p| InboundGroupSession::from_pickle(p).ok()).collect())
    }

    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        let redis_key = format!("{}inbound_group_sessions", self.key_prefix);
        let mut connection = self.client.get_async_connection().await?;
        let igss: Vec<String> = connection.hvals(&redis_key).await?;

        let pickles: Result<Vec<PickledInboundGroupSession>> =
            igss.iter().map(|p| self.deserialize_value(p.as_bytes())).collect();

        let pickles = pickles?;

        let total = pickles.len();
        let backed_up = pickles.into_iter().filter(|p| p.backed_up).count();

        Ok(RoomKeyCounts { total, backed_up })
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        let redis_key = format!("{}inbound_group_sessions", self.key_prefix);
        let mut connection = self.client.get_async_connection().await?;
        let igss: Vec<String> = connection.hvals(&redis_key).await?;

        let pickles = igss
            .iter()
            .map(|p| self.deserialize_value(p.as_bytes()))
            .filter_map(|p: Result<PickledInboundGroupSession, CryptoStoreError>| match p {
                Ok(p) => {
                    if !p.backed_up {
                        Some(InboundGroupSession::from_pickle(p).map_err(CryptoStoreError::from))
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

    fn users_for_key_query(&self) -> HashSet<OwnedUserId> {
        self.users_for_key_query_cache.iter().map(|u| u.clone()).collect()
    }

    fn tracked_users(&self) -> HashSet<OwnedUserId> {
        self.tracked_users_cache.to_owned().iter().map(|u| u.clone()).collect()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        let already_added = self.tracked_users_cache.insert(user.to_owned());

        if dirty {
            self.users_for_key_query_cache.insert(user.to_owned());
        } else {
            self.users_for_key_query_cache.remove(user);
        }

        self.save_tracked_users(&[(user, dirty)]).await?;

        Ok(already_added)
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        let mut connection = self.client.get_async_connection().await?;
        let key = format!("{}devices|{user_id}", self.key_prefix);
        let dev: Option<Vec<u8>> = connection.hget(&key, device_id.as_str()).await?;
        Ok(dev.map(|d| self.deserialize_value(&d)).transpose()?)
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        let mut connection = self.client.get_async_connection().await?;
        let user_device: HashMap<String, Vec<u8>> =
            connection.hgetall(&format!("{}devices|{user_id}", self.key_prefix)).await?;

        user_device
            .into_iter()
            .map(|(device_id, device_str)| {
                let d = self.deserialize_value(&device_str)?;
                Ok((device_id.into(), d))
            })
            .collect()
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
        let mut connection = self.client.get_async_connection().await?;
        let redis_key = format!("{}identities|{user_id}", self.key_prefix);
        let identity_string: Option<Vec<u8>> = connection.get(&redis_key).await?;
        let identity = identity_string.map(|s| self.deserialize_value(&s)).transpose()?;
        Ok(identity)
    }

    async fn is_message_known(
        &self,
        message_hash: &matrix_sdk_crypto::olm::OlmMessageHash,
    ) -> Result<bool> {
        let mut connection = self.client.get_async_connection().await?;
        let redis_key = format!("{}olm_hashes", self.key_prefix);
        let ret = connection.sismember(&redis_key, &serde_json::to_string(message_hash)?).await?;
        Ok(ret)
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>> {
        self.get_outgoing_key_request_helper(&request_id.redis_key()).await
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let mut connection = self.client.get_async_connection().await.unwrap();
        let redis_key =
            format!("{}secret_requests_by_info|{}", self.key_prefix, key_info.redis_key());
        let id: Option<String> = connection.get(&redis_key).await.unwrap();

        if let Some(id) = id {
            self.get_outgoing_key_request_helper(&id).await
        } else {
            Ok(None)
        }
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        let mut connection = self.client.get_async_connection().await.unwrap();
        let redis_key = format!("{}unsent_secret_requests", self.key_prefix);
        let req_map: HashMap<String, Vec<u8>> = connection.hgetall(&redis_key).await.unwrap();
        Ok(req_map.values().map(|req| self.deserialize_value(req).unwrap()).collect())
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        let mut connection = self.client.get_async_connection().await?;
        let okr_req_id_key =
            format!("{}outgoing_secret_requests|{}", self.key_prefix, request_id.redis_key());
        let sent_request: Option<Vec<u8>> = connection.get(&okr_req_id_key).await?;

        // Wrap the deletes in a Redis transaction
        // TODO: race: if someone updates sent_request before we delete it, we
        // could be deleting the old stuff, when others are using a newer version,
        // so we would be in an inconsistent state where the sent_request is deleted,
        // but the things it refers to still exists.
        let mut pipeline = self.client.create_pipe();
        if let Some(sent_request) = sent_request {
            pipeline.del(&okr_req_id_key);
            let usr_key = format!("{}unsent_secret_requests", self.key_prefix);
            pipeline.hdel(&usr_key, &request_id.redis_key());
            let sent_request: GossipRequest = self.deserialize_value(&sent_request)?;
            let srbi_info_key = format!(
                "{}secret_requests_by_info|{}",
                self.key_prefix,
                sent_request.info.redis_key()
            );
            pipeline.del(&srbi_info_key);
        }
        pipeline.query_async(&mut connection).await?;

        Ok(())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        let mut connection = self.client.get_async_connection().await?;
        let redis_key = format!("{}backup_version_v1", self.key_prefix);
        let version_v: Option<Vec<u8>> = connection.get(&redis_key).await?;
        let version = version_v.map(|v| self.deserialize_value(&v)).transpose()?;

        let redis_key = format!("{}recovery_key_v1", self.key_prefix);
        let recovery_key_str: Option<Vec<u8>> = connection.get(&redis_key).await?;
        let recovery_key: Option<RecoveryKey> =
            recovery_key_str.map(|s| self.deserialize_value(&s)).transpose()?;

        Ok(BackupKeys { backup_version: version, recovery_key })
    }
}

#[cfg(test)]
mod test_fake_redis {
    use std::{collections::HashMap, sync::Arc};

    use matrix_sdk_crypto::cryptostore_integration_tests;
    use once_cell::sync::Lazy;
    use redis::{ConnectionAddr, ConnectionInfo, RedisConnectionInfo};
    use tokio::sync::Mutex;

    use super::RedisStore;
    use crate::fake_redis::FakeRedisClient;

    static REDIS_CLIENT: Lazy<FakeRedisClient> = Lazy::new(FakeRedisClient::new);

    async fn get_store(name: &str, passphrase: Option<&str>) -> RedisStore<FakeRedisClient> {
        let key_prefix = format!("matrix-sdk-crypto|test|{name}|");
        RedisStore::open(REDIS_CLIENT.clone(), passphrase, key_prefix)
            .await
            .expect("Can't create a Redis store")
    }

    cryptostore_integration_tests! {}
}

// To run tests against a real Redis, use:
// ```sh
// cargo test redis --features=real-redis-tests
// ```
#[cfg(feature = "real-redis-tests")]
#[cfg(test)]
mod test_real_redis {
    use matrix_sdk_crypto::cryptostore_integration_tests;
    use once_cell::sync::Lazy;
    use redis::{AsyncCommands, Client, Commands};

    use super::RedisStore;
    use crate::real_redis::RealRedisClient;

    static REDIS_URL: &str = "redis://127.0.0.1/";

    // We pretend to use this as our shared client, so that
    // we clear Redis the first time we access it, but actually
    // we clone it each time we use it, so they are independent.
    static REDIS_CLIENT: Lazy<Client> = Lazy::new(|| {
        let client = Client::open(REDIS_URL).unwrap();
        let mut connection = client.get_connection().unwrap();
        let keys: Vec<String> = connection.keys("matrix-sdk-crypto|test|*").unwrap();
        for k in keys {
            let _: () = connection.del(k).unwrap();
        }
        client
    });

    async fn get_store(name: &str, passphrase: Option<&str>) -> RedisStore<RealRedisClient> {
        let key_prefix = format!("matrix-sdk-crypto|test|{}|", name);
        let redis_client = RealRedisClient::from(REDIS_CLIENT.clone());
        let store = RedisStore::open(redis_client, passphrase, key_prefix)
            .await
            .expect("Can't create a Redis store");

        store
    }

    cryptostore_integration_tests! {}
}
