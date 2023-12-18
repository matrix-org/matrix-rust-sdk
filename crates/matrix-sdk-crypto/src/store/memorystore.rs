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
    collections::{hash_map::Entry, HashMap, HashSet},
    convert::Infallible,
    sync::{Arc, RwLock as StdRwLock},
    time::{Duration, Instant},
};

use async_trait::async_trait;
use ruma::{
    events::secret::request::SecretName, DeviceId, OwnedDeviceId, OwnedRoomId, OwnedTransactionId,
    OwnedUserId, RoomId, TransactionId, UserId,
};
use tokio::sync::{Mutex, RwLock};
use tracing::warn;

use super::{
    caches::{DeviceStore, GroupSessionStore, SessionStore},
    Account, BackupKeys, Changes, CryptoStore, InboundGroupSession, PendingChanges, RoomKeyCounts,
    RoomSettings, Session,
};
use crate::{
    gossiping::{GossipRequest, GossippedSecret, SecretInfo},
    identities::{ReadOnlyDevice, ReadOnlyUserIdentities},
    olm::{OutboundGroupSession, PrivateCrossSigningIdentity},
    types::events::room_key_withheld::RoomKeyWithheldEvent,
    TrackedUser,
};

fn encode_key_info(info: &SecretInfo) -> String {
    match info {
        SecretInfo::KeyRequest(info) => {
            format!("{}{}{}", info.room_id(), info.algorithm(), info.session_id())
        }
        SecretInfo::SecretRequest(i) => i.as_ref().to_owned(),
    }
}

/// An in-memory only store that will forget all the E2EE key once it's dropped.
#[derive(Debug)]
pub struct MemoryStore {
    account: StdRwLock<Option<Account>>,
    sessions: SessionStore,
    inbound_group_sessions: GroupSessionStore,
    olm_hashes: StdRwLock<HashMap<String, HashSet<String>>>,
    devices: DeviceStore,
    identities: StdRwLock<HashMap<OwnedUserId, ReadOnlyUserIdentities>>,
    outgoing_key_requests: StdRwLock<HashMap<OwnedTransactionId, GossipRequest>>,
    key_requests_by_info: StdRwLock<HashMap<String, OwnedTransactionId>>,
    direct_withheld_info: StdRwLock<HashMap<OwnedRoomId, HashMap<String, RoomKeyWithheldEvent>>>,
    custom_values: StdRwLock<HashMap<String, Vec<u8>>>,
    leases: StdRwLock<HashMap<String, (String, Instant)>>,
    secret_inbox: StdRwLock<HashMap<String, Vec<GossippedSecret>>>,
    backup_keys: RwLock<BackupKeys>,
    next_batch_token: RwLock<Option<String>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore {
            account: Default::default(),
            sessions: SessionStore::new(),
            inbound_group_sessions: GroupSessionStore::new(),
            olm_hashes: Default::default(),
            devices: DeviceStore::new(),
            identities: Default::default(),
            outgoing_key_requests: Default::default(),
            key_requests_by_info: Default::default(),
            direct_withheld_info: Default::default(),
            custom_values: Default::default(),
            leases: Default::default(),
            backup_keys: Default::default(),
            secret_inbox: Default::default(),
            next_batch_token: Default::default(),
        }
    }
}

impl MemoryStore {
    /// Create a new empty `MemoryStore`.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn save_devices(&self, devices: Vec<ReadOnlyDevice>) {
        for device in devices {
            let _ = self.devices.add(device);
        }
    }

    fn delete_devices(&self, devices: Vec<ReadOnlyDevice>) {
        for device in devices {
            let _ = self.devices.remove(device.user_id(), device.device_id());
        }
    }

    async fn save_sessions(&self, sessions: Vec<Session>) {
        for session in sessions {
            let _ = self.sessions.add(session.clone()).await;
        }
    }

    fn save_inbound_group_sessions(&self, sessions: Vec<InboundGroupSession>) {
        for session in sessions {
            self.inbound_group_sessions.add(session);
        }
    }
}

type Result<T> = std::result::Result<T, Infallible>;

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl CryptoStore for MemoryStore {
    type Error = Infallible;

    async fn load_account(&self) -> Result<Option<Account>> {
        Ok(self.account.read().unwrap().as_ref().map(|acc| acc.deep_clone()))
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        Ok(None)
    }

    async fn next_batch_token(&self) -> Result<Option<String>> {
        Ok(self.next_batch_token.read().await.clone())
    }

    async fn save_pending_changes(&self, changes: PendingChanges) -> Result<()> {
        if let Some(account) = changes.account {
            *self.account.write().unwrap() = Some(account);
        }

        Ok(())
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        self.save_sessions(changes.sessions).await;
        self.save_inbound_group_sessions(changes.inbound_group_sessions);

        self.save_devices(changes.devices.new);
        self.save_devices(changes.devices.changed);
        self.delete_devices(changes.devices.deleted);

        {
            let mut identities = self.identities.write().unwrap();
            for identity in changes.identities.new.into_iter().chain(changes.identities.changed) {
                identities.insert(identity.user_id().to_owned(), identity.clone());
            }
        }

        {
            let mut olm_hashes = self.olm_hashes.write().unwrap();
            for hash in changes.message_hashes {
                olm_hashes.entry(hash.sender_key.to_owned()).or_default().insert(hash.hash.clone());
            }
        }

        {
            let mut outgoing_key_requests = self.outgoing_key_requests.write().unwrap();
            let mut key_requests_by_info = self.key_requests_by_info.write().unwrap();

            for key_request in changes.key_requests {
                let id = key_request.request_id.clone();
                let info_string = encode_key_info(&key_request.info);

                outgoing_key_requests.insert(id.clone(), key_request);
                key_requests_by_info.insert(info_string, id);
            }
        }

        if let Some(key) = changes.backup_decryption_key {
            self.backup_keys.write().await.decryption_key = Some(key);
        }

        if let Some(version) = changes.backup_version {
            self.backup_keys.write().await.backup_version = Some(version);
        }

        {
            let mut secret_inbox = self.secret_inbox.write().unwrap();
            for secret in changes.secrets {
                secret_inbox.entry(secret.secret_name.to_string()).or_default().push(secret);
            }
        }

        {
            let mut direct_withheld_info = self.direct_withheld_info.write().unwrap();
            for (room_id, data) in changes.withheld_session_info {
                for (session_id, event) in data {
                    direct_withheld_info
                        .entry(room_id.to_owned())
                        .or_default()
                        .insert(session_id, event);
                }
            }
        }

        if let Some(next_batch_token) = changes.next_batch_token {
            *self.next_batch_token.write().await = Some(next_batch_token);
        }

        Ok(())
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        Ok(self.sessions.get(sender_key))
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        Ok(self.inbound_group_sessions.get(room_id, session_id))
    }

    async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEvent>> {
        Ok(self
            .direct_withheld_info
            .read()
            .unwrap()
            .get(room_id)
            .and_then(|e| Some(e.get(session_id)?.to_owned())))
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        Ok(self.inbound_group_sessions.get_all())
    }

    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        let backed_up =
            self.get_inbound_group_sessions().await?.into_iter().filter(|s| s.backed_up()).count();

        Ok(RoomKeyCounts { total: self.inbound_group_sessions.count(), backed_up })
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

    async fn mark_inbound_group_sessions_as_backed_up(
        &self,
        room_and_session_ids: &[(&RoomId, &str)],
    ) -> Result<()> {
        for (room_id, session_id) in room_and_session_ids {
            let session = self.inbound_group_sessions.get(room_id, session_id);
            if let Some(session) = session {
                session.mark_as_backed_up();
                self.inbound_group_sessions.add(session);
            }
        }
        Ok(())
    }

    async fn reset_backup_state(&self) -> Result<()> {
        for session in self.get_inbound_group_sessions().await? {
            session.reset_backup_state();
        }

        Ok(())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        Ok(self.backup_keys.read().await.to_owned())
    }

    async fn get_outbound_group_session(&self, _: &RoomId) -> Result<Option<OutboundGroupSession>> {
        Ok(None)
    }

    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>> {
        Ok(Vec::new())
    }

    async fn save_tracked_users(&self, _: &[(&UserId, bool)]) -> Result<()> {
        Ok(())
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>> {
        Ok(self.devices.get(user_id, device_id))
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>> {
        Ok(self.devices.user_devices(user_id))
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
        Ok(self.identities.read().unwrap().get(user_id).cloned())
    }

    async fn is_message_known(&self, message_hash: &crate::olm::OlmMessageHash) -> Result<bool> {
        Ok(self
            .olm_hashes
            .write()
            .unwrap()
            .entry(message_hash.sender_key.to_owned())
            .or_default()
            .contains(&message_hash.hash))
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>> {
        Ok(self.outgoing_key_requests.read().unwrap().get(request_id).cloned())
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let key_info_string = encode_key_info(key_info);

        Ok(self
            .key_requests_by_info
            .read()
            .unwrap()
            .get(&key_info_string)
            .and_then(|i| self.outgoing_key_requests.read().unwrap().get(i).cloned()))
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        Ok(self
            .outgoing_key_requests
            .read()
            .unwrap()
            .values()
            .filter(|req| !req.sent_out)
            .cloned()
            .collect())
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        let req = self.outgoing_key_requests.write().unwrap().remove(request_id);
        if let Some(i) = req {
            let key_info_string = encode_key_info(&i.info);
            self.key_requests_by_info.write().unwrap().remove(&key_info_string);
        }

        Ok(())
    }

    async fn get_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
    ) -> Result<Vec<GossippedSecret>> {
        Ok(self
            .secret_inbox
            .write()
            .unwrap()
            .entry(secret_name.to_string())
            .or_default()
            .to_owned())
    }

    async fn delete_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<()> {
        self.secret_inbox.write().unwrap().remove(secret_name.as_str());

        Ok(())
    }

    async fn get_room_settings(&self, _room_id: &RoomId) -> Result<Option<RoomSettings>> {
        warn!("Method not implemented");
        Ok(None)
    }

    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.custom_values.read().unwrap().get(key).cloned())
    }

    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<()> {
        self.custom_values.write().unwrap().insert(key.to_owned(), value);
        Ok(())
    }

    async fn remove_custom_value(&self, key: &str) -> Result<()> {
        self.custom_values.write().unwrap().remove(key);
        Ok(())
    }

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool> {
        let now = Instant::now();
        let expiration = now + Duration::from_millis(lease_duration_ms.into());
        match self.leases.write().unwrap().entry(key.to_owned()) {
            Entry::Occupied(mut o) => {
                let prev = o.get_mut();
                if prev.0 == holder {
                    // We had the lease before, extend it.
                    prev.1 = expiration;
                    Ok(true)
                } else {
                    // We didn't have it.
                    if prev.1 < now {
                        // Steal it!
                        prev.0 = holder.to_owned();
                        prev.1 = expiration;
                        Ok(true)
                    } else {
                        // We tried our best.
                        Ok(false)
                    }
                }
            }
            Entry::Vacant(v) => {
                v.insert((
                    holder.to_owned(),
                    Instant::now() + Duration::from_millis(lease_duration_ms.into()),
                ));
                Ok(true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::room_id;
    use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

    use crate::{
        identities::device::testing::get_device,
        olm::{tests::get_account_and_session_test_helper, InboundGroupSession, OlmMessageHash},
        store::{memorystore::MemoryStore, Changes, CryptoStore, PendingChanges},
    };

    #[async_test]
    async fn test_session_store() {
        let (account, session) = get_account_and_session_test_helper();
        let store = MemoryStore::new();

        assert!(store.load_account().await.unwrap().is_none());
        store.save_pending_changes(PendingChanges { account: Some(account) }).await.unwrap();

        store.save_sessions(vec![session.clone()]).await;

        let sessions = store.get_sessions(&session.sender_key.to_base64()).await.unwrap().unwrap();
        let sessions = sessions.lock().await;

        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[async_test]
    async fn test_group_session_store() {
        let (account, _) = get_account_and_session_test_helper();
        let room_id = room_id!("!test:localhost");
        let curve_key = "Nn0L2hkcCMFKqynTjyGsJbth7QrVmX3lbrksMkrGOAw";

        let (outbound, _) = account.create_group_session_pair_with_defaults(room_id).await;
        let inbound = InboundGroupSession::new(
            Curve25519PublicKey::from_base64(curve_key).unwrap(),
            Ed25519PublicKey::from_base64("ee3Ek+J2LkkPmjGPGLhMxiKnhiX//xcqaVL4RP6EypE").unwrap(),
            room_id,
            &outbound.session_key().await,
            outbound.settings().algorithm.to_owned(),
            None,
        )
        .unwrap();

        let store = MemoryStore::new();
        store.save_inbound_group_sessions(vec![inbound.clone()]);

        let loaded_session =
            store.get_inbound_group_session(room_id, outbound.session_id()).await.unwrap().unwrap();
        assert_eq!(inbound, loaded_session);
    }

    #[async_test]
    async fn test_device_store() {
        let device = get_device();
        let store = MemoryStore::new();

        store.save_devices(vec![device.clone()]);

        let loaded_device =
            store.get_device(device.user_id(), device.device_id()).await.unwrap().unwrap();

        assert_eq!(device, loaded_device);

        let user_devices = store.get_user_devices(device.user_id()).await.unwrap();

        assert_eq!(&**user_devices.keys().next().unwrap(), device.device_id());
        assert_eq!(user_devices.values().next().unwrap(), &device);

        let loaded_device = user_devices.get(device.device_id()).unwrap();

        assert_eq!(&device, loaded_device);

        store.delete_devices(vec![device.clone()]);
        assert!(store.get_device(device.user_id(), device.device_id()).await.unwrap().is_none());
    }

    #[async_test]
    async fn test_message_hash() {
        let store = MemoryStore::new();

        let hash =
            OlmMessageHash { sender_key: "test_sender".to_owned(), hash: "test_hash".to_owned() };

        let mut changes = Changes::default();
        changes.message_hashes.push(hash.clone());

        assert!(!store.is_message_known(&hash).await.unwrap());
        store.save_changes(changes).await.unwrap();
        assert!(store.is_message_known(&hash).await.unwrap());
    }
}
