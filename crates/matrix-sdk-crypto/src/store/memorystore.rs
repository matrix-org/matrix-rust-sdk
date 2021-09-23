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
    sync::Arc,
};

use dashmap::{DashMap, DashSet};
use matrix_sdk_common::{async_trait, locks::Mutex, uuid::Uuid};
use ruma::{DeviceId, DeviceIdBox, RoomId, UserId};

use super::{
    caches::{DeviceStore, GroupSessionStore, SessionStore},
    BackupKeys, Changes, CryptoStore, InboundGroupSession, ReadOnlyAccount, Result, RoomKeyCounts,
    Session,
};
use crate::{
    gossiping::{GossipRequest, SecretInfo},
    identities::{ReadOnlyDevice, ReadOnlyUserIdentities},
    olm::{OutboundGroupSession, PrivateCrossSigningIdentity},
};

fn encode_key_info(info: &SecretInfo) -> String {
    match info {
        SecretInfo::KeyRequest(info) => {
            format!("{}{}{}{}", info.room_id, info.sender_key, info.algorithm, info.session_id)
        }
        SecretInfo::SecretRequest(i) => i.as_ref().to_owned(),
    }
}

/// An in-memory only store that will forget all the E2EE key once it's dropped.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    sessions: SessionStore,
    inbound_group_sessions: GroupSessionStore,
    tracked_users: Arc<DashSet<UserId>>,
    users_for_key_query: Arc<DashSet<UserId>>,
    olm_hashes: Arc<DashMap<String, DashSet<String>>>,
    devices: DeviceStore,
    identities: Arc<DashMap<UserId, ReadOnlyUserIdentities>>,
    outgoing_key_requests: Arc<DashMap<Uuid, GossipRequest>>,
    key_requests_by_info: Arc<DashMap<String, Uuid>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore {
            sessions: SessionStore::new(),
            inbound_group_sessions: GroupSessionStore::new(),
            tracked_users: Default::default(),
            users_for_key_query: Default::default(),
            olm_hashes: Default::default(),
            devices: DeviceStore::new(),
            identities: Default::default(),
            outgoing_key_requests: Default::default(),
            key_requests_by_info: Default::default(),
        }
    }
}

impl MemoryStore {
    /// Create a new empty `MemoryStore`.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) async fn save_devices(&self, mut devices: Vec<ReadOnlyDevice>) {
        for device in devices.drain(..) {
            let _ = self.devices.add(device);
        }
    }

    async fn delete_devices(&self, mut devices: Vec<ReadOnlyDevice>) {
        for device in devices.drain(..) {
            let _ = self.devices.remove(device.user_id(), device.device_id());
        }
    }

    async fn save_sessions(&self, mut sessions: Vec<Session>) {
        for session in sessions.drain(..) {
            let _ = self.sessions.add(session.clone()).await;
        }
    }

    async fn save_inbound_group_sessions(&self, mut sessions: Vec<InboundGroupSession>) {
        for session in sessions.drain(..) {
            self.inbound_group_sessions.add(session);
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl CryptoStore for MemoryStore {
    async fn load_account(&self) -> Result<Option<ReadOnlyAccount>> {
        Ok(None)
    }

    async fn save_account(&self, _: ReadOnlyAccount) -> Result<()> {
        Ok(())
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        Ok(None)
    }

    async fn save_changes(&self, mut changes: Changes) -> Result<()> {
        self.save_sessions(changes.sessions).await;
        self.save_inbound_group_sessions(changes.inbound_group_sessions).await;

        self.save_devices(changes.devices.new).await;
        self.save_devices(changes.devices.changed).await;
        self.delete_devices(changes.devices.deleted).await;

        for identity in changes.identities.new.drain(..).chain(changes.identities.changed) {
            let _ = self.identities.insert(identity.user_id().to_owned(), identity.clone());
        }

        for hash in changes.message_hashes {
            self.olm_hashes
                .entry(hash.sender_key.to_owned())
                .or_insert_with(DashSet::new)
                .insert(hash.hash.clone());
        }

        for key_request in changes.key_requests {
            let id = key_request.request_id;
            let info_string = encode_key_info(&key_request.info);

            self.outgoing_key_requests.insert(id, key_request);
            self.key_requests_by_info.insert(info_string, id);
        }

        Ok(())
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        Ok(self.sessions.get(sender_key))
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        Ok(self.inbound_group_sessions.get(room_id, sender_key, session_id))
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        Ok(self.inbound_group_sessions.get_all())
    }

    async fn inbound_group_session_counts(&self) -> Result<RoomKeyCounts> {
        let backed_up = self.inbound_group_sessions_for_backup().await?.len();

        Ok(RoomKeyCounts { total: self.inbound_group_sessions.count(), backed_up })
    }

    async fn inbound_group_sessions_for_backup(&self) -> Result<Vec<InboundGroupSession>> {
        Ok(self
            .get_inbound_group_sessions()
            .await?
            .into_iter()
            .filter(|s| !s.backed_up())
            .collect())
    }

    async fn reset_backup_state(&self) -> Result<()> {
        for session in self.get_inbound_group_sessions().await? {
            session.reset_backup_state()
        }

        Ok(())
    }

    async fn get_outbound_group_sessions(
        &self,
        _: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        Ok(None)
    }

    fn is_user_tracked(&self, user_id: &UserId) -> bool {
        self.tracked_users.contains(user_id)
    }

    fn has_users_for_key_query(&self) -> bool {
        !self.users_for_key_query.is_empty()
    }

    fn users_for_key_query(&self) -> HashSet<UserId> {
        self.users_for_key_query.iter().map(|u| u.clone()).collect()
    }

    fn tracked_users(&self) -> HashSet<UserId> {
        self.tracked_users.iter().map(|u| u.to_owned()).collect()
    }

    async fn update_tracked_user(&self, user: &UserId, dirty: bool) -> Result<bool> {
        // TODO to prevent a race between the sync and a key query in flight we
        // need to have an additional state to mention that the user changed.
        //
        // A simple counter could be used for this or enum with two states, e.g.
        // The counter would work as follows:
        // * 0 -> User is synced, no need for a key query.
        // * 1 -> A sync has marked the user as dirty.
        // * 2 -> A sync has marked the user again as dirty, before we got a
        // successful key query response.
        //
        // The counter would top out at 2 since there won't be a race between 3
        // different key queries syncs.
        if dirty {
            self.users_for_key_query.insert(user.clone());
        } else {
            self.users_for_key_query.remove(user);
        }

        Ok(self.tracked_users.insert(user.clone()))
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
    ) -> Result<HashMap<DeviceIdBox, ReadOnlyDevice>> {
        Ok(self.devices.user_devices(user_id))
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<ReadOnlyUserIdentities>> {
        Ok(self.identities.get(user_id).map(|i| i.clone()))
    }

    async fn is_message_known(&self, message_hash: &crate::olm::OlmMessageHash) -> Result<bool> {
        Ok(self
            .olm_hashes
            .entry(message_hash.sender_key.to_owned())
            .or_insert_with(DashSet::new)
            .contains(&message_hash.hash))
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: Uuid,
    ) -> Result<Option<GossipRequest>> {
        Ok(self.outgoing_key_requests.get(&request_id).map(|r| r.clone()))
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let key_info_string = encode_key_info(key_info);

        Ok(self
            .key_requests_by_info
            .get(&key_info_string)
            .and_then(|i| self.outgoing_key_requests.get(&i).map(|r| r.clone())))
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        Ok(self
            .outgoing_key_requests
            .iter()
            .filter(|i| !i.value().sent_out)
            .map(|i| i.value().clone())
            .collect())
    }

    async fn delete_outgoing_secret_requests(&self, request_id: Uuid) -> Result<()> {
        self.outgoing_key_requests.remove(&request_id).and_then(|(_, i)| {
            let key_info_string = encode_key_info(&i.info);
            self.key_requests_by_info.remove(&key_info_string)
        });

        Ok(())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        todo!()
    }
}

#[cfg(test)]
mod test {
    use ruma::room_id;

    use crate::{
        identities::device::test::get_device,
        olm::{test::get_account_and_session, InboundGroupSession, OlmMessageHash},
        store::{memorystore::MemoryStore, Changes, CryptoStore},
    };

    #[tokio::test]
    async fn test_session_store() {
        let (account, session) = get_account_and_session().await;
        let store = MemoryStore::new();

        assert!(store.load_account().await.unwrap().is_none());
        store.save_account(account).await.unwrap();

        store.save_sessions(vec![session.clone()]).await;

        let sessions = store.get_sessions(&session.sender_key).await.unwrap().unwrap();
        let sessions = sessions.lock().await;

        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[tokio::test]
    async fn test_group_session_store() {
        let (account, _) = get_account_and_session().await;
        let room_id = room_id!("!test:localhost");

        let (outbound, _) =
            account.create_group_session_pair_with_defaults(&room_id).await.unwrap();
        let inbound = InboundGroupSession::new(
            "test_key",
            "test_key",
            &room_id,
            outbound.session_key().await,
            None,
        )
        .unwrap();

        let store = MemoryStore::new();
        let _ = store.save_inbound_group_sessions(vec![inbound.clone()]).await;

        let loaded_session = store
            .get_inbound_group_session(&room_id, "test_key", outbound.session_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(inbound, loaded_session);
    }

    #[tokio::test]
    async fn test_device_store() {
        let device = get_device();
        let store = MemoryStore::new();

        store.save_devices(vec![device.clone()]).await;

        let loaded_device =
            store.get_device(device.user_id(), device.device_id()).await.unwrap().unwrap();

        assert_eq!(device, loaded_device);

        let user_devices = store.get_user_devices(device.user_id()).await.unwrap();

        assert_eq!(&**user_devices.keys().next().unwrap(), device.device_id());
        assert_eq!(user_devices.values().next().unwrap(), &device);

        let loaded_device = user_devices.get(device.device_id()).unwrap();

        assert_eq!(&device, loaded_device);

        store.delete_devices(vec![device.clone()]).await;
        assert!(store.get_device(device.user_id(), device.device_id()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_tracked_users() {
        let device = get_device();
        let store = MemoryStore::new();

        assert!(store.update_tracked_user(device.user_id(), false).await.unwrap());
        assert!(!store.update_tracked_user(device.user_id(), false).await.unwrap());

        assert!(store.is_user_tracked(device.user_id()));
    }

    #[tokio::test]
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
