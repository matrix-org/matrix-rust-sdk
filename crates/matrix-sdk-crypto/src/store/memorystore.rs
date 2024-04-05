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
    collections::{hash_map::Entry, BTreeMap, HashMap, HashSet},
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
    outbound_group_sessions: StdRwLock<BTreeMap<OwnedRoomId, OutboundGroupSession>>,
    private_identity: StdRwLock<Option<PrivateCrossSigningIdentity>>,
    tracked_users: StdRwLock<HashMap<OwnedUserId, TrackedUser>>,
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
    room_settings: StdRwLock<HashMap<OwnedRoomId, RoomSettings>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        MemoryStore {
            account: Default::default(),
            sessions: SessionStore::new(),
            inbound_group_sessions: GroupSessionStore::new(),
            outbound_group_sessions: Default::default(),
            private_identity: Default::default(),
            tracked_users: Default::default(),
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
            room_settings: Default::default(),
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

    fn save_outbound_group_sessions(&self, sessions: Vec<OutboundGroupSession>) {
        self.outbound_group_sessions
            .write()
            .unwrap()
            .extend(sessions.into_iter().map(|s| (s.room_id().to_owned(), s)));
    }

    fn save_private_identity(&self, private_identity: Option<PrivateCrossSigningIdentity>) {
        *self.private_identity.write().unwrap() = private_identity;
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
        Ok(self.private_identity.read().unwrap().clone())
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
        self.save_outbound_group_sessions(changes.outbound_group_sessions);
        self.save_private_identity(changes.private_identity);

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

        if !changes.room_settings.is_empty() {
            let mut settings = self.room_settings.write().unwrap();
            settings.extend(changes.room_settings);
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

    async fn inbound_group_session_counts(
        &self,
        _backup_version: Option<&str>,
    ) -> Result<RoomKeyCounts> {
        let backed_up =
            self.get_inbound_group_sessions().await?.into_iter().filter(|s| s.backed_up()).count();

        Ok(RoomKeyCounts { total: self.inbound_group_sessions.count(), backed_up })
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        _backup_version: &str,
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
        _backup_version: &str,
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

    async fn get_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        Ok(self.outbound_group_sessions.read().unwrap().get(room_id).cloned())
    }

    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>> {
        Ok(self.tracked_users.read().unwrap().values().cloned().collect())
    }

    async fn save_tracked_users(&self, tracked_users: &[(&UserId, bool)]) -> Result<()> {
        self.tracked_users.write().unwrap().extend(tracked_users.iter().map(|(user_id, dirty)| {
            let user_id: OwnedUserId = user_id.to_owned().into();
            (user_id.clone(), TrackedUser { user_id, dirty: *dirty })
        }));
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

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>> {
        Ok(self.room_settings.read().unwrap().get(room_id).cloned())
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
    use ruma::{room_id, user_id};
    use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

    use crate::{
        identities::device::testing::get_device,
        olm::{
            tests::get_account_and_session_test_helper, InboundGroupSession, OlmMessageHash,
            PrivateCrossSigningIdentity,
        },
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
    async fn test_inbound_group_session_store() {
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
    async fn test_outbound_group_session_store() {
        // Given an outbound session
        let (account, _) = get_account_and_session_test_helper();
        let room_id = room_id!("!test:localhost");
        let (outbound, _) = account.create_group_session_pair_with_defaults(room_id).await;

        // When we save it to the store
        let store = MemoryStore::new();
        store.save_outbound_group_sessions(vec![outbound.clone()]);

        // Then we can get it out again
        let loaded_session = store.get_outbound_group_session(room_id).await.unwrap().unwrap();
        assert_eq!(
            serde_json::to_string(&outbound.pickle().await).unwrap(),
            serde_json::to_string(&loaded_session.pickle().await).unwrap()
        );
    }

    #[async_test]
    async fn test_tracked_users_are_stored_once_per_user_id() {
        // Given a store containing 2 tracked users, both dirty
        let user1 = user_id!("@user1:s");
        let user2 = user_id!("@user2:s");
        let user3 = user_id!("@user3:s");
        let store = MemoryStore::new();
        store.save_tracked_users(&[(user1, true), (user2, true)]).await.unwrap();

        // When we mark one as clean and add another
        store.save_tracked_users(&[(user2, false), (user3, false)]).await.unwrap();

        // Then we can get them out again and their dirty flags are correct
        let loaded_tracked_users =
            store.load_tracked_users().await.expect("failed to load tracked users");

        let tracked_contains = |user_id, dirty| {
            loaded_tracked_users.iter().any(|u| u.user_id == user_id && u.dirty == dirty)
        };

        assert!(tracked_contains(user1, true));
        assert!(tracked_contains(user2, false));
        assert!(tracked_contains(user3, false));
        assert_eq!(loaded_tracked_users.len(), 3);
    }

    #[async_test]
    async fn test_private_identity_store() {
        // Given a private identity
        let private_identity = PrivateCrossSigningIdentity::empty(user_id!("@u:s"));

        // When we save it to the store
        let store = MemoryStore::new();
        store.save_private_identity(Some(private_identity.clone()));

        // Then we can get it out again
        let loaded_identity =
            store.load_identity().await.expect("failed to load private identity").unwrap();

        assert_eq!(loaded_identity.user_id(), user_id!("@u:s"));
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

#[cfg(test)]
mod integration_tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, OnceLock},
    };

    use async_trait::async_trait;
    use ruma::{
        events::secret::request::SecretName, DeviceId, OwnedDeviceId, RoomId, TransactionId, UserId,
    };

    use super::MemoryStore;
    use crate::{
        cryptostore_integration_tests, cryptostore_integration_tests_time,
        olm::{
            InboundGroupSession, OlmMessageHash, OutboundGroupSession, PrivateCrossSigningIdentity,
            StaticAccountData,
        },
        store::{BackupKeys, Changes, CryptoStore, PendingChanges, RoomKeyCounts, RoomSettings},
        types::events::room_key_withheld::RoomKeyWithheldEvent,
        Account, GossipRequest, GossippedSecret, ReadOnlyDevice, ReadOnlyUserIdentities,
        SecretInfo, Session, TrackedUser,
    };

    /// Holds on to a MemoryStore during a test, and moves it back into STORES
    /// when this is dropped
    #[derive(Clone, Debug)]
    struct PersistentMemoryStore(Arc<MemoryStore>);

    impl PersistentMemoryStore {
        fn new() -> Self {
            Self(Arc::new(MemoryStore::new()))
        }

        fn get_static_account(&self) -> Option<StaticAccountData> {
            self.0.get_static_account()
        }
    }

    impl MemoryStore {
        fn get_static_account(&self) -> Option<StaticAccountData> {
            self.account.read().unwrap().as_ref().map(|acc| acc.static_data().clone())
        }
    }

    /// Return a clone of the store for the test with the supplied name. Note:
    /// dropping this store won't destroy its data, since
    /// [PersistentMemoryStore] is a reference-counted smart pointer
    /// to an underlying [MemoryStore].
    async fn get_store(name: &str, _passphrase: Option<&str>) -> PersistentMemoryStore {
        // Holds on to one [PersistentMemoryStore] per test, so even if the test drops
        // the store, we keep its data alive. This simulates the behaviour of
        // the other stores, which keep their data in a real DB, allowing us to
        // test MemoryStore using the same code.
        static STORES: OnceLock<Mutex<HashMap<String, PersistentMemoryStore>>> = OnceLock::new();
        let stores = STORES.get_or_init(|| Mutex::new(HashMap::new()));

        stores
            .lock()
            .unwrap()
            .entry(name.to_owned())
            .or_insert_with(PersistentMemoryStore::new)
            .clone()
    }

    /// Forwards all methods to the underlying [MemoryStore].
    #[async_trait]
    impl CryptoStore for PersistentMemoryStore {
        type Error = <MemoryStore as CryptoStore>::Error;

        async fn load_account(&self) -> Result<Option<Account>, Self::Error> {
            self.0.load_account().await
        }

        async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>, Self::Error> {
            self.0.load_identity().await
        }

        async fn save_changes(&self, changes: Changes) -> Result<(), Self::Error> {
            self.0.save_changes(changes).await
        }

        async fn save_pending_changes(&self, changes: PendingChanges) -> Result<(), Self::Error> {
            self.0.save_pending_changes(changes).await
        }

        async fn get_sessions(
            &self,
            sender_key: &str,
        ) -> Result<Option<Arc<tokio::sync::Mutex<Vec<Session>>>>, Self::Error> {
            self.0.get_sessions(sender_key).await
        }

        async fn get_inbound_group_session(
            &self,
            room_id: &RoomId,
            session_id: &str,
        ) -> Result<Option<InboundGroupSession>, Self::Error> {
            self.0.get_inbound_group_session(room_id, session_id).await
        }

        async fn get_withheld_info(
            &self,
            room_id: &RoomId,
            session_id: &str,
        ) -> Result<Option<RoomKeyWithheldEvent>, Self::Error> {
            self.0.get_withheld_info(room_id, session_id).await
        }

        async fn get_inbound_group_sessions(
            &self,
        ) -> Result<Vec<InboundGroupSession>, Self::Error> {
            self.0.get_inbound_group_sessions().await
        }

        async fn inbound_group_session_counts(
            &self,
            backup_version: Option<&str>,
        ) -> Result<RoomKeyCounts, Self::Error> {
            self.0.inbound_group_session_counts(backup_version).await
        }

        async fn inbound_group_sessions_for_backup(
            &self,
            backup_version: &str,
            limit: usize,
        ) -> Result<Vec<InboundGroupSession>, Self::Error> {
            self.0.inbound_group_sessions_for_backup(backup_version, limit).await
        }

        async fn mark_inbound_group_sessions_as_backed_up(
            &self,
            backup_version: &str,
            room_and_session_ids: &[(&RoomId, &str)],
        ) -> Result<(), Self::Error> {
            self.0
                .mark_inbound_group_sessions_as_backed_up(backup_version, room_and_session_ids)
                .await
        }

        async fn reset_backup_state(&self) -> Result<(), Self::Error> {
            self.0.reset_backup_state().await
        }

        async fn load_backup_keys(&self) -> Result<BackupKeys, Self::Error> {
            self.0.load_backup_keys().await
        }

        async fn get_outbound_group_session(
            &self,
            room_id: &RoomId,
        ) -> Result<Option<OutboundGroupSession>, Self::Error> {
            self.0.get_outbound_group_session(room_id).await
        }

        async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>, Self::Error> {
            self.0.load_tracked_users().await
        }

        async fn save_tracked_users(&self, users: &[(&UserId, bool)]) -> Result<(), Self::Error> {
            self.0.save_tracked_users(users).await
        }

        async fn get_device(
            &self,
            user_id: &UserId,
            device_id: &DeviceId,
        ) -> Result<Option<ReadOnlyDevice>, Self::Error> {
            self.0.get_device(user_id, device_id).await
        }

        async fn get_user_devices(
            &self,
            user_id: &UserId,
        ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>, Self::Error> {
            self.0.get_user_devices(user_id).await
        }

        async fn get_user_identity(
            &self,
            user_id: &UserId,
        ) -> Result<Option<ReadOnlyUserIdentities>, Self::Error> {
            self.0.get_user_identity(user_id).await
        }

        async fn is_message_known(
            &self,
            message_hash: &OlmMessageHash,
        ) -> Result<bool, Self::Error> {
            self.0.is_message_known(message_hash).await
        }

        async fn get_outgoing_secret_requests(
            &self,
            request_id: &TransactionId,
        ) -> Result<Option<GossipRequest>, Self::Error> {
            self.0.get_outgoing_secret_requests(request_id).await
        }

        async fn get_secret_request_by_info(
            &self,
            secret_info: &SecretInfo,
        ) -> Result<Option<GossipRequest>, Self::Error> {
            self.0.get_secret_request_by_info(secret_info).await
        }

        async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>, Self::Error> {
            self.0.get_unsent_secret_requests().await
        }

        async fn delete_outgoing_secret_requests(
            &self,
            request_id: &TransactionId,
        ) -> Result<(), Self::Error> {
            self.0.delete_outgoing_secret_requests(request_id).await
        }

        async fn get_secrets_from_inbox(
            &self,
            secret_name: &SecretName,
        ) -> Result<Vec<GossippedSecret>, Self::Error> {
            self.0.get_secrets_from_inbox(secret_name).await
        }

        async fn delete_secrets_from_inbox(
            &self,
            secret_name: &SecretName,
        ) -> Result<(), Self::Error> {
            self.0.delete_secrets_from_inbox(secret_name).await
        }

        async fn get_room_settings(
            &self,
            room_id: &RoomId,
        ) -> Result<Option<RoomSettings>, Self::Error> {
            self.0.get_room_settings(room_id).await
        }

        async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>, Self::Error> {
            self.0.get_custom_value(key).await
        }

        async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<(), Self::Error> {
            self.0.set_custom_value(key, value).await
        }

        async fn remove_custom_value(&self, key: &str) -> Result<(), Self::Error> {
            self.0.remove_custom_value(key).await
        }

        async fn try_take_leased_lock(
            &self,
            lease_duration_ms: u32,
            key: &str,
            holder: &str,
        ) -> Result<bool, Self::Error> {
            self.0.try_take_leased_lock(lease_duration_ms, key, holder).await
        }

        async fn next_batch_token(&self) -> Result<Option<String>, Self::Error> {
            self.0.next_batch_token().await
        }
    }

    cryptostore_integration_tests!();
    cryptostore_integration_tests_time!();
}
