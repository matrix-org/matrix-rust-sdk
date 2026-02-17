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
    convert::Infallible,
    sync::Arc,
};

use async_trait::async_trait;
use matrix_sdk_common::{
    cross_process_lock::{
        CrossProcessLockGeneration,
        memory_store_helper::{Lease, try_take_leased_lock},
    },
    locks::RwLock as StdRwLock,
};
use ruma::{
    DeviceId, OwnedDeviceId, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId, TransactionId,
    UserId, events::secret::request::SecretName,
};
use tokio::sync::{Mutex, RwLock};
use tracing::warn;
use vodozemac::Curve25519PublicKey;

use super::{
    Account, CryptoStore, InboundGroupSession, Session,
    caches::DeviceStore,
    types::{
        BackupKeys, Changes, DehydratedDeviceKey, PendingChanges, RoomKeyCounts, RoomSettings,
        StoredRoomKeyBundleData, TrackedUser,
    },
};
use crate::{
    gossiping::{GossipRequest, GossippedSecret, SecretInfo},
    identities::{DeviceData, UserIdentityData},
    olm::{
        OutboundGroupSession, PickledAccount, PickledInboundGroupSession, PickledSession,
        PrivateCrossSigningIdentity, SenderDataType, StaticAccountData,
    },
    store::types::RoomKeyWithheldEntry,
};

fn encode_key_info(info: &SecretInfo) -> String {
    match info {
        SecretInfo::KeyRequest(info) => {
            format!("{}{}{}", info.room_id(), info.algorithm(), info.session_id())
        }
        SecretInfo::SecretRequest(i) => i.as_ref().to_owned(),
    }
}

type SessionId = String;

/// The "version" of a backup - newtype wrapper around a String.
#[derive(Clone, Debug, PartialEq)]
struct BackupVersion(String);

impl BackupVersion {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

/// An in-memory only store that will forget all the E2EE key once it's dropped.
#[derive(Default, Debug)]
pub struct MemoryStore {
    static_account: Arc<StdRwLock<Option<StaticAccountData>>>,

    account: StdRwLock<Option<String>>,
    // Map of sender_key to map of session_id to serialized pickle
    sessions: StdRwLock<BTreeMap<String, BTreeMap<String, String>>>,
    inbound_group_sessions: StdRwLock<BTreeMap<OwnedRoomId, HashMap<String, String>>>,

    /// Map room id -> session id -> backup order number
    /// The latest backup in which this session is stored. Equivalent to
    /// `backed_up_to` in [`IndexedDbCryptoStore`]
    inbound_group_sessions_backed_up_to:
        StdRwLock<HashMap<OwnedRoomId, HashMap<SessionId, BackupVersion>>>,

    outbound_group_sessions: StdRwLock<BTreeMap<OwnedRoomId, OutboundGroupSession>>,
    private_identity: StdRwLock<Option<PrivateCrossSigningIdentity>>,
    tracked_users: StdRwLock<HashMap<OwnedUserId, TrackedUser>>,
    olm_hashes: StdRwLock<HashMap<String, HashSet<String>>>,
    devices: DeviceStore,
    identities: StdRwLock<HashMap<OwnedUserId, String>>,
    outgoing_key_requests: StdRwLock<HashMap<OwnedTransactionId, GossipRequest>>,
    key_requests_by_info: StdRwLock<HashMap<String, OwnedTransactionId>>,
    direct_withheld_info: StdRwLock<HashMap<OwnedRoomId, HashMap<String, RoomKeyWithheldEntry>>>,
    custom_values: StdRwLock<HashMap<String, Vec<u8>>>,
    leases: StdRwLock<HashMap<String, Lease>>,
    secret_inbox: StdRwLock<HashMap<String, Vec<GossippedSecret>>>,
    backup_keys: RwLock<BackupKeys>,
    dehydrated_device_pickle_key: RwLock<Option<DehydratedDeviceKey>>,
    next_batch_token: RwLock<Option<String>>,
    room_settings: StdRwLock<HashMap<OwnedRoomId, RoomSettings>>,
    room_key_bundles:
        StdRwLock<HashMap<OwnedRoomId, HashMap<OwnedUserId, StoredRoomKeyBundleData>>>,
    room_key_backups_fully_downloaded: StdRwLock<HashSet<OwnedRoomId>>,

    save_changes_lock: Arc<Mutex<()>>,
}

impl MemoryStore {
    /// Create a new empty `MemoryStore`.
    pub fn new() -> Self {
        Self::default()
    }

    fn get_static_account(&self) -> Option<StaticAccountData> {
        self.static_account.read().clone()
    }

    pub(crate) fn save_devices(&self, devices: Vec<DeviceData>) {
        for device in devices {
            let _ = self.devices.add(device);
        }
    }

    fn delete_devices(&self, devices: Vec<DeviceData>) {
        for device in devices {
            let _ = self.devices.remove(device.user_id(), device.device_id());
        }
    }

    fn save_sessions(&self, sessions: Vec<(String, PickledSession)>) {
        let mut session_store = self.sessions.write();

        for (session_id, pickle) in sessions {
            let entry = session_store.entry(pickle.sender_key.to_base64()).or_default();

            // insert or replace if exists
            entry.insert(
                session_id,
                serde_json::to_string(&pickle).expect("Failed to serialize olm session"),
            );
        }
    }

    fn save_outbound_group_sessions(&self, sessions: Vec<OutboundGroupSession>) {
        self.outbound_group_sessions
            .write()
            .extend(sessions.into_iter().map(|s| (s.room_id().to_owned(), s)));
    }

    fn save_private_identity(&self, private_identity: Option<PrivateCrossSigningIdentity>) {
        *self.private_identity.write() = private_identity;
    }

    /// Return all the [`InboundGroupSession`]s we have, paired with the
    /// `backed_up_to` value for each one (or "" where it is missing, which
    /// should never happen).
    async fn get_inbound_group_sessions_and_backed_up_to(
        &self,
    ) -> Result<Vec<(InboundGroupSession, Option<BackupVersion>)>> {
        let lookup = |s: &InboundGroupSession| {
            self.inbound_group_sessions_backed_up_to
                .read()
                .get(&s.room_id)?
                .get(s.session_id())
                .cloned()
        };

        Ok(self
            .get_inbound_group_sessions()
            .await?
            .into_iter()
            .map(|s| {
                let v = lookup(&s);
                (s, v)
            })
            .collect())
    }
}

type Result<T> = std::result::Result<T, Infallible>;

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl CryptoStore for MemoryStore {
    type Error = Infallible;

    async fn load_account(&self) -> Result<Option<Account>> {
        let pickled_account: Option<PickledAccount> = self.account.read().as_ref().map(|acc| {
            serde_json::from_str(acc)
                .expect("Deserialization failed: invalid pickled account JSON format")
        });

        if let Some(pickle) = pickled_account {
            let account =
                Account::from_pickle(pickle).expect("From pickle failed: invalid pickle format");

            *self.static_account.write() = Some(account.static_data().clone());

            Ok(Some(account))
        } else {
            Ok(None)
        }
    }

    async fn load_identity(&self) -> Result<Option<PrivateCrossSigningIdentity>> {
        Ok(self.private_identity.read().clone())
    }

    async fn next_batch_token(&self) -> Result<Option<String>> {
        Ok(self.next_batch_token.read().await.clone())
    }

    async fn save_pending_changes(&self, changes: PendingChanges) -> Result<()> {
        let _guard = self.save_changes_lock.lock().await;

        let pickled_account = if let Some(account) = changes.account {
            *self.static_account.write() = Some(account.static_data().clone());
            Some(account.pickle())
        } else {
            None
        };

        *self.account.write() = pickled_account.map(|pickle| {
            serde_json::to_string(&pickle)
                .expect("Serialization failed: invalid pickled account JSON format")
        });

        Ok(())
    }

    async fn save_changes(&self, changes: Changes) -> Result<()> {
        let _guard = self.save_changes_lock.lock().await;

        let mut pickled_session: Vec<(String, PickledSession)> = Vec::new();
        for session in changes.sessions {
            let session_id = session.session_id().to_owned();
            let pickle = session.pickle().await;
            pickled_session.push((session_id.clone(), pickle));
        }
        self.save_sessions(pickled_session);

        self.save_inbound_group_sessions(changes.inbound_group_sessions, None).await?;
        self.save_outbound_group_sessions(changes.outbound_group_sessions);
        self.save_private_identity(changes.private_identity);

        self.save_devices(changes.devices.new);
        self.save_devices(changes.devices.changed);
        self.delete_devices(changes.devices.deleted);

        {
            let mut identities = self.identities.write();
            for identity in changes.identities.new.into_iter().chain(changes.identities.changed) {
                identities.insert(
                    identity.user_id().to_owned(),
                    serde_json::to_string(&identity)
                        .expect("UserIdentityData should always serialize to json"),
                );
            }
        }

        {
            let mut olm_hashes = self.olm_hashes.write();
            for hash in changes.message_hashes {
                olm_hashes.entry(hash.sender_key.to_owned()).or_default().insert(hash.hash.clone());
            }
        }

        {
            let mut outgoing_key_requests = self.outgoing_key_requests.write();
            let mut key_requests_by_info = self.key_requests_by_info.write();

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

        if let Some(pickle_key) = changes.dehydrated_device_pickle_key {
            let mut lock = self.dehydrated_device_pickle_key.write().await;
            *lock = Some(pickle_key);
        }

        {
            let mut secret_inbox = self.secret_inbox.write();
            for secret in changes.secrets {
                secret_inbox.entry(secret.secret_name.to_string()).or_default().push(secret);
            }
        }

        {
            let mut direct_withheld_info = self.direct_withheld_info.write();
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
            let mut settings = self.room_settings.write();
            settings.extend(changes.room_settings);
        }

        if !changes.received_room_key_bundles.is_empty() {
            let mut room_key_bundles = self.room_key_bundles.write();
            for bundle in changes.received_room_key_bundles {
                room_key_bundles
                    .entry(bundle.bundle_data.room_id.clone())
                    .or_default()
                    .insert(bundle.sender_user.clone(), bundle);
            }
        }

        if !changes.room_key_backups_fully_downloaded.is_empty() {
            let mut room_key_backups_fully_downloaded =
                self.room_key_backups_fully_downloaded.write();
            for room in changes.room_key_backups_fully_downloaded {
                room_key_backups_fully_downloaded.insert(room);
            }
        }

        Ok(())
    }

    async fn save_inbound_group_sessions(
        &self,
        sessions: Vec<InboundGroupSession>,
        backed_up_to_version: Option<&str>,
    ) -> Result<()> {
        for session in sessions {
            let room_id = session.room_id();
            let session_id = session.session_id();

            // Sanity-check that the data in the sessions corresponds to backed_up_version
            let backed_up = session.backed_up();
            if backed_up != backed_up_to_version.is_some() {
                warn!(
                    backed_up,
                    backed_up_to_version,
                    "Session backed-up flag does not correspond to backup version setting",
                );
            }

            if let Some(backup_version) = backed_up_to_version {
                self.inbound_group_sessions_backed_up_to
                    .write()
                    .entry(room_id.to_owned())
                    .or_default()
                    .insert(session_id.to_owned(), BackupVersion::from(backup_version));
            }

            let pickle = session.pickle().await;
            self.inbound_group_sessions
                .write()
                .entry(session.room_id().to_owned())
                .or_default()
                .insert(
                    session.session_id().to_owned(),
                    serde_json::to_string(&pickle)
                        .expect("Pickle pickle data should serialize to json"),
                );
        }
        Ok(())
    }

    async fn get_sessions(&self, sender_key: &str) -> Result<Option<Vec<Session>>> {
        let device_keys = self.get_own_device().await?.as_device_keys().clone();

        if let Some(pickles) = self.sessions.read().get(sender_key) {
            let mut sessions: Vec<Session> = Vec::new();
            for serialized_pickle in pickles.values() {
                let pickle: PickledSession = serde_json::from_str(serialized_pickle.as_str())
                    .expect("Pickle pickle deserialization should work");
                let session = Session::from_pickle(device_keys.clone(), pickle)
                    .expect("Expect from pickle to always work");
                sessions.push(session);
            }
            Ok(Some(sessions))
        } else {
            Ok(None)
        }
    }

    async fn get_inbound_group_session(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        let pickle: Option<PickledInboundGroupSession> = self
            .inbound_group_sessions
            .read()
            .get(room_id)
            .and_then(|m| m.get(session_id))
            .and_then(|ser| {
                serde_json::from_str(ser).expect("Pickle pickle deserialization should work")
            });

        Ok(pickle.map(|p| {
            InboundGroupSession::from_pickle(p).expect("Expect from pickle to always work")
        }))
    }

    async fn get_withheld_info(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<RoomKeyWithheldEntry>> {
        Ok(self
            .direct_withheld_info
            .read()
            .get(room_id)
            .and_then(|e| Some(e.get(session_id)?.to_owned())))
    }

    async fn get_withheld_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> crate::store::Result<Vec<RoomKeyWithheldEntry>, Self::Error> {
        Ok(self
            .direct_withheld_info
            .read()
            .get(room_id)
            .map(|e| e.values().cloned().collect())
            .unwrap_or_default())
    }

    async fn get_inbound_group_sessions(&self) -> Result<Vec<InboundGroupSession>> {
        let inbounds = self
            .inbound_group_sessions
            .read()
            .values()
            .flat_map(HashMap::values)
            .map(|ser| {
                let pickle: PickledInboundGroupSession =
                    serde_json::from_str(ser).expect("Pickle deserialization should work");
                InboundGroupSession::from_pickle(pickle).expect("Expect from pickle to always work")
            })
            .collect();
        Ok(inbounds)
    }

    async fn inbound_group_session_counts(
        &self,
        backup_version: Option<&str>,
    ) -> Result<RoomKeyCounts> {
        let backed_up = if let Some(backup_version) = backup_version {
            self.get_inbound_group_sessions_and_backed_up_to()
                .await?
                .into_iter()
                // Count the sessions backed up in the required backup
                .filter(|(_, o)| o.as_ref().is_some_and(|o| o.as_str() == backup_version))
                .count()
        } else {
            // We asked about a nonexistent backup version - this doesn't make much sense,
            // but we can easily answer that nothing is backed up in this
            // nonexistent backup.
            0
        };

        let total = self.inbound_group_sessions.read().values().map(HashMap::len).sum();
        Ok(RoomKeyCounts { total, backed_up })
    }

    async fn get_inbound_group_sessions_by_room_id(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<InboundGroupSession>> {
        let inbounds = match self.inbound_group_sessions.read().get(room_id) {
            None => Vec::new(),
            Some(v) => v
                .values()
                .map(|ser| {
                    let pickle: PickledInboundGroupSession =
                        serde_json::from_str(ser).expect("Pickle deserialization should work");
                    InboundGroupSession::from_pickle(pickle)
                        .expect("Expect from pickle to always work")
                })
                .collect(),
        };
        Ok(inbounds)
    }

    async fn get_inbound_group_sessions_for_device_batch(
        &self,
        sender_key: Curve25519PublicKey,
        sender_data_type: SenderDataType,
        after_session_id: Option<String>,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        // First, find all InboundGroupSessions, filtering for those that match the
        // device and sender_data type.
        let mut sessions: Vec<_> = self
            .get_inbound_group_sessions()
            .await?
            .into_iter()
            .filter(|session: &InboundGroupSession| {
                session.creator_info.curve25519_key == sender_key
                    && session.sender_data.to_type() == sender_data_type
            })
            .collect();

        // Then, sort the sessions in order of ascending session ID...
        sessions.sort_by_key(|s| s.session_id().to_owned());

        // Figure out where in the array to start returning results from
        let start_index = {
            match after_session_id {
                None => 0,
                Some(id) => {
                    // We're looking for the first session with a session ID strictly after `id`; if
                    // there are none, the end of the array.
                    sessions
                        .iter()
                        .position(|session| session.session_id() > id.as_str())
                        .unwrap_or(sessions.len())
                }
            }
        };

        // Return up to `limit` items from the array, starting from `start_index`
        Ok(sessions.drain(start_index..).take(limit).collect())
    }

    async fn inbound_group_sessions_for_backup(
        &self,
        backup_version: &str,
        limit: usize,
    ) -> Result<Vec<InboundGroupSession>> {
        Ok(self
            .get_inbound_group_sessions_and_backed_up_to()
            .await?
            .into_iter()
            .filter_map(|(session, backed_up_to)| {
                if let Some(ref existing_version) = backed_up_to
                    && existing_version.as_str() == backup_version
                {
                    // This session is already backed up in the required backup
                    None
                } else {
                    // It's not backed up, or it's backed up in a different backup
                    Some(session)
                }
            })
            .take(limit)
            .collect())
    }

    async fn mark_inbound_group_sessions_as_backed_up(
        &self,
        backup_version: &str,
        room_and_session_ids: &[(&RoomId, &str)],
    ) -> Result<()> {
        for &(room_id, session_id) in room_and_session_ids {
            let session = self.get_inbound_group_session(room_id, session_id).await?;

            if let Some(session) = session {
                session.mark_as_backed_up();

                self.inbound_group_sessions_backed_up_to
                    .write()
                    .entry(room_id.to_owned())
                    .or_default()
                    .insert(session_id.to_owned(), BackupVersion::from(backup_version));

                // Save it back
                let updated_pickle = session.pickle().await;

                self.inbound_group_sessions.write().entry(room_id.to_owned()).or_default().insert(
                    session_id.to_owned(),
                    serde_json::to_string(&updated_pickle)
                        .expect("Pickle serialization should work"),
                );
            }
        }

        Ok(())
    }

    async fn reset_backup_state(&self) -> Result<()> {
        // Nothing to do here, because we remember which backup versions we backed up to
        // in `mark_inbound_group_sessions_as_backed_up`, so we don't need to
        // reset anything here because the required version is passed in to
        // `inbound_group_sessions_for_backup`, and we can compare against the
        // version we stored.

        Ok(())
    }

    async fn load_backup_keys(&self) -> Result<BackupKeys> {
        Ok(self.backup_keys.read().await.to_owned())
    }

    async fn load_dehydrated_device_pickle_key(&self) -> Result<Option<DehydratedDeviceKey>> {
        Ok(self.dehydrated_device_pickle_key.read().await.to_owned())
    }

    async fn delete_dehydrated_device_pickle_key(&self) -> Result<()> {
        let mut lock = self.dehydrated_device_pickle_key.write().await;
        *lock = None;
        Ok(())
    }

    async fn get_outbound_group_session(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<OutboundGroupSession>> {
        Ok(self.outbound_group_sessions.read().get(room_id).cloned())
    }

    async fn load_tracked_users(&self) -> Result<Vec<TrackedUser>> {
        Ok(self.tracked_users.read().values().cloned().collect())
    }

    async fn save_tracked_users(&self, tracked_users: &[(&UserId, bool)]) -> Result<()> {
        self.tracked_users.write().extend(tracked_users.iter().map(|(user_id, dirty)| {
            let user_id: OwnedUserId = user_id.to_owned().into();
            (user_id.clone(), TrackedUser { user_id, dirty: *dirty })
        }));
        Ok(())
    }

    async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<DeviceData>> {
        Ok(self.devices.get(user_id, device_id))
    }

    async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, DeviceData>> {
        Ok(self.devices.user_devices(user_id))
    }

    async fn get_own_device(&self) -> Result<DeviceData> {
        let account =
            self.get_static_account().expect("Expect account to exist when getting own device");

        Ok(self
            .devices
            .get(&account.user_id, &account.device_id)
            .expect("Invalid state: Should always have a own device"))
    }

    async fn get_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentityData>> {
        let serialized = self.identities.read().get(user_id).cloned();
        match serialized {
            None => Ok(None),
            Some(serialized) => {
                let id: UserIdentityData = serde_json::from_str(serialized.as_str())
                    .expect("Only valid serialized identity are saved");
                Ok(Some(id))
            }
        }
    }

    async fn is_message_known(&self, message_hash: &crate::olm::OlmMessageHash) -> Result<bool> {
        Ok(self
            .olm_hashes
            .write()
            .entry(message_hash.sender_key.to_owned())
            .or_default()
            .contains(&message_hash.hash))
    }

    async fn get_outgoing_secret_requests(
        &self,
        request_id: &TransactionId,
    ) -> Result<Option<GossipRequest>> {
        Ok(self.outgoing_key_requests.read().get(request_id).cloned())
    }

    async fn get_secret_request_by_info(
        &self,
        key_info: &SecretInfo,
    ) -> Result<Option<GossipRequest>> {
        let key_info_string = encode_key_info(key_info);

        Ok(self
            .key_requests_by_info
            .read()
            .get(&key_info_string)
            .and_then(|i| self.outgoing_key_requests.read().get(i).cloned()))
    }

    async fn get_unsent_secret_requests(&self) -> Result<Vec<GossipRequest>> {
        Ok(self
            .outgoing_key_requests
            .read()
            .values()
            .filter(|req| !req.sent_out)
            .cloned()
            .collect())
    }

    async fn delete_outgoing_secret_requests(&self, request_id: &TransactionId) -> Result<()> {
        let req = self.outgoing_key_requests.write().remove(request_id);
        if let Some(i) = req {
            let key_info_string = encode_key_info(&i.info);
            self.key_requests_by_info.write().remove(&key_info_string);
        }

        Ok(())
    }

    async fn get_secrets_from_inbox(
        &self,
        secret_name: &SecretName,
    ) -> Result<Vec<GossippedSecret>> {
        Ok(self.secret_inbox.write().entry(secret_name.to_string()).or_default().to_owned())
    }

    async fn delete_secrets_from_inbox(&self, secret_name: &SecretName) -> Result<()> {
        self.secret_inbox.write().remove(secret_name.as_str());

        Ok(())
    }

    async fn get_room_settings(&self, room_id: &RoomId) -> Result<Option<RoomSettings>> {
        Ok(self.room_settings.read().get(room_id).cloned())
    }

    async fn get_received_room_key_bundle_data(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<StoredRoomKeyBundleData>> {
        let guard = self.room_key_bundles.read();

        let result = guard.get(room_id).and_then(|bundles| bundles.get(user_id).cloned());

        Ok(result)
    }

    async fn has_downloaded_all_room_keys(&self, room_id: &RoomId) -> Result<bool> {
        let guard = self.room_key_backups_fully_downloaded.read();
        Ok(guard.contains(room_id))
    }

    async fn get_custom_value(&self, key: &str) -> Result<Option<Vec<u8>>> {
        Ok(self.custom_values.read().get(key).cloned())
    }

    async fn set_custom_value(&self, key: &str, value: Vec<u8>) -> Result<()> {
        self.custom_values.write().insert(key.to_owned(), value);
        Ok(())
    }

    async fn remove_custom_value(&self, key: &str) -> Result<()> {
        self.custom_values.write().remove(key);
        Ok(())
    }

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>> {
        Ok(try_take_leased_lock(&mut self.leases.write(), lease_duration_ms, key, holder))
    }

    async fn get_size(&self) -> Result<Option<usize>> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use matrix_sdk_test::async_test;
    use ruma::{RoomId, room_id, user_id};
    use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

    use super::SessionId;
    use crate::{
        DeviceData,
        identities::device::testing::get_device,
        olm::{
            Account, InboundGroupSession, OlmMessageHash, PrivateCrossSigningIdentity, SenderData,
            tests::get_account_and_session_test_helper,
        },
        store::{
            CryptoStore,
            memorystore::MemoryStore,
            types::{Changes, DeviceChanges, PendingChanges},
        },
    };

    #[async_test]
    async fn test_session_store() {
        let (account, session) = get_account_and_session_test_helper();
        let own_device = DeviceData::from_account(&account);
        let store = MemoryStore::new();

        assert!(store.load_account().await.unwrap().is_none());

        store
            .save_changes(Changes {
                devices: DeviceChanges { new: vec![own_device], ..Default::default() },
                ..Default::default()
            })
            .await
            .unwrap();
        store.save_pending_changes(PendingChanges { account: Some(account) }).await.unwrap();

        store
            .save_changes(Changes { sessions: (vec![session.clone()]), ..Default::default() })
            .await
            .unwrap();

        let sessions = store.get_sessions(&session.sender_key.to_base64()).await.unwrap().unwrap();

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
            SenderData::unknown(),
            None,
            outbound.settings().algorithm.to_owned(),
            None,
            false,
        )
        .unwrap();

        let store = MemoryStore::new();
        store.save_inbound_group_sessions(vec![inbound.clone()], None).await.unwrap();

        let loaded_session =
            store.get_inbound_group_session(room_id, outbound.session_id()).await.unwrap().unwrap();
        assert_eq!(inbound, loaded_session);
    }

    #[async_test]
    async fn test_backing_up_marks_sessions_as_backed_up() {
        // Given there are 2 sessions
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(2, room_id).await;

        // When I mark them as backed up
        mark_backed_up(&store, room_id, "bkp1", &sessions).await;

        // Then their backed_up_to field is set
        let but = backed_up_tos(&store).await;
        assert_eq!(but[sessions[0].session_id()], "bkp1");
        assert_eq!(but[sessions[1].session_id()], "bkp1");
    }

    #[async_test]
    async fn test_backing_up_a_second_set_of_sessions_updates_their_backup_order() {
        // Given there are 3 sessions
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(3, room_id).await;

        // When I mark 0 and 1 as backed up in bkp1
        mark_backed_up(&store, room_id, "bkp1", &sessions[..2]).await;

        // And 1 and 2 as backed up in bkp2
        mark_backed_up(&store, room_id, "bkp2", &sessions[1..]).await;

        // Then 0 is backed up in bkp1 and the 1 and 2 are backed up in bkp2
        let but = backed_up_tos(&store).await;
        assert_eq!(but[sessions[0].session_id()], "bkp1");
        assert_eq!(but[sessions[1].session_id()], "bkp2");
        assert_eq!(but[sessions[2].session_id()], "bkp2");
    }

    #[async_test]
    async fn test_backing_up_again_to_the_same_version_has_no_effect() {
        // Given there are 3 sessions
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(3, room_id).await;

        // When I mark the first two as backed up in the first backup
        mark_backed_up(&store, room_id, "bkp1", &sessions[..2]).await;

        // And the last 2 as backed up in the same backup version
        mark_backed_up(&store, room_id, "bkp1", &sessions[1..]).await;

        // Then they all get the same backed_up_to value
        let but = backed_up_tos(&store).await;
        assert_eq!(but[sessions[0].session_id()], "bkp1");
        assert_eq!(but[sessions[1].session_id()], "bkp1");
        assert_eq!(but[sessions[2].session_id()], "bkp1");
    }

    #[async_test]
    async fn test_backing_up_to_an_old_backup_version_can_increase_backed_up_to() {
        // Given we have backed up some sessions to 2 backup versions, an older and a
        // newer
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(4, room_id).await;
        mark_backed_up(&store, room_id, "older_bkp", &sessions[..2]).await;
        mark_backed_up(&store, room_id, "newer_bkp", &sessions[1..2]).await;

        // When I ask to back up the un-backed-up ones to the older backup
        mark_backed_up(&store, room_id, "older_bkp", &sessions[2..]).await;

        // Then each session lists the backup it was most recently included in
        let but = backed_up_tos(&store).await;
        assert_eq!(but[sessions[0].session_id()], "older_bkp");
        assert_eq!(but[sessions[1].session_id()], "newer_bkp");
        assert_eq!(but[sessions[2].session_id()], "older_bkp");
        assert_eq!(but[sessions[3].session_id()], "older_bkp");
    }

    #[async_test]
    async fn test_backing_up_to_an_old_backup_version_overwrites_a_newer_one() {
        // Given we have backed up to 2 backup versions, an older and a newer
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(4, room_id).await;
        mark_backed_up(&store, room_id, "older_bkp", &sessions).await;
        // Sanity: they are backed up in order number 1
        assert_eq!(backed_up_tos(&store).await[sessions[0].session_id()], "older_bkp");
        mark_backed_up(&store, room_id, "newer_bkp", &sessions).await;
        // Sanity: they are backed up in order number 2
        assert_eq!(backed_up_tos(&store).await[sessions[0].session_id()], "newer_bkp");

        // When I ask to back up some to the older version
        mark_backed_up(&store, room_id, "older_bkp", &sessions[..2]).await;

        // Then older backup overwrites: we don't consider the order here at all
        let but = backed_up_tos(&store).await;
        assert_eq!(but[sessions[0].session_id()], "older_bkp");
        assert_eq!(but[sessions[1].session_id()], "older_bkp");
        assert_eq!(but[sessions[2].session_id()], "newer_bkp");
        assert_eq!(but[sessions[3].session_id()], "newer_bkp");
    }

    #[async_test]
    async fn test_not_backed_up_sessions_are_eligible_for_backup() {
        // Given there are 4 sessions, 2 of which are already backed up
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(4, room_id).await;
        mark_backed_up(&store, room_id, "bkp1", &sessions[..2]).await;

        // When I ask which to back up
        let mut to_backup = store
            .inbound_group_sessions_for_backup("bkp1", 10)
            .await
            .expect("Failed to ask for sessions to backup");
        to_backup.sort_by_key(|s| s.session_id().to_owned());

        // Then I am told the last 2 only
        assert_eq!(to_backup, &[sessions[2].clone(), sessions[3].clone()]);
    }

    #[async_test]
    async fn test_all_sessions_are_eligible_for_backup_if_version_is_unknown() {
        // Given there are 4 sessions, 2 of which are already backed up in bkp1
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(4, room_id).await;
        mark_backed_up(&store, room_id, "bkp1", &sessions[..2]).await;

        // When I ask which to back up in an unknown version
        let mut to_backup = store
            .inbound_group_sessions_for_backup("unknown_bkp", 10)
            .await
            .expect("Failed to ask for sessions to backup");
        to_backup.sort_by_key(|s| s.session_id().to_owned());

        // Then I am told to back up all of them
        assert_eq!(
            to_backup,
            &[sessions[0].clone(), sessions[1].clone(), sessions[2].clone(), sessions[3].clone()]
        );
    }

    #[async_test]
    async fn test_sessions_backed_up_to_a_later_version_are_eligible_for_backup() {
        // Given there are 4 sessions, some backed up to three different versions
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(4, room_id).await;
        mark_backed_up(&store, room_id, "bkp0", &sessions[..1]).await;
        mark_backed_up(&store, room_id, "bkp1", &sessions[1..2]).await;
        mark_backed_up(&store, room_id, "bkp2", &sessions[2..3]).await;

        // When I ask which to back up in the middle version
        let mut to_backup = store
            .inbound_group_sessions_for_backup("bkp1", 10)
            .await
            .expect("Failed to ask for sessions to backup");
        to_backup.sort_by_key(|s| s.session_id().to_owned());

        // Then I am told to back up everything not in the version I asked about
        assert_eq!(
            to_backup,
            &[
                sessions[0].clone(), // Backed up in bkp0
                // sessions[1] is backed up in bkp1 already, which we asked about
                sessions[2].clone(), // Backed up in bkp2
                sessions[3].clone(), // Not backed up
            ]
        );
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

    #[async_test]
    async fn test_key_counts_of_empty_store_are_zero() {
        // Given an empty store
        let store = MemoryStore::new();

        // When we count keys
        let key_counts = store.inbound_group_session_counts(Some("")).await.unwrap();

        // Then the answer is zero
        assert_eq!(key_counts.total, 0);
        assert_eq!(key_counts.backed_up, 0);
    }

    #[async_test]
    async fn test_counting_sessions_reports_the_number_of_sessions() {
        // Given a store with sessions
        let room_id = room_id!("!test:localhost");
        let (store, _) = store_with_sessions(4, room_id).await;

        // When we count keys
        let key_counts = store.inbound_group_session_counts(Some("bkp")).await.unwrap();

        // Then the answer equals the number of sessions we created
        assert_eq!(key_counts.total, 4);
        // And none are backed up
        assert_eq!(key_counts.backed_up, 0);
    }

    #[async_test]
    async fn test_counting_backed_up_sessions_reports_the_number_backed_up_in_this_backup() {
        // Given a store with sessions, some backed up
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(5, room_id).await;
        mark_backed_up(&store, room_id, "bkp", &sessions[..2]).await;

        // When we count keys
        let key_counts = store.inbound_group_session_counts(Some("bkp")).await.unwrap();

        // Then the answer equals the number of sessions we created
        assert_eq!(key_counts.total, 5);
        // And the backed_up count matches how many were backed up
        assert_eq!(key_counts.backed_up, 2);
    }

    #[async_test]
    async fn test_counting_backed_up_sessions_for_null_backup_reports_zero() {
        // Given a store with sessions, some backed up
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(4, room_id).await;
        mark_backed_up(&store, room_id, "bkp", &sessions[..2]).await;

        // When we count keys, providing None as the backup version
        let key_counts = store.inbound_group_session_counts(None).await.unwrap();

        // Then we ignore everything and just say zero
        assert_eq!(key_counts.backed_up, 0);
    }

    #[async_test]
    async fn test_counting_backed_up_sessions_only_reports_sessions_in_the_version_specified() {
        // Given a store with sessions, backed up in several versions
        let room_id = room_id!("!test:localhost");
        let (store, sessions) = store_with_sessions(4, room_id).await;
        mark_backed_up(&store, room_id, "bkp1", &sessions[..2]).await;
        mark_backed_up(&store, room_id, "bkp2", &sessions[3..]).await;

        // When we count keys for bkp2
        let key_counts = store.inbound_group_session_counts(Some("bkp2")).await.unwrap();

        // Then the backed_up count reflects how many were backed up in bkp2 only
        assert_eq!(key_counts.backed_up, 1);
    }

    /// Mark the supplied sessions as backed up in the supplied backup version
    async fn mark_backed_up(
        store: &MemoryStore,
        room_id: &RoomId,
        backup_version: &str,
        sessions: &[InboundGroupSession],
    ) {
        let rooms_and_ids: Vec<_> = sessions.iter().map(|s| (room_id, s.session_id())).collect();

        store
            .mark_inbound_group_sessions_as_backed_up(backup_version, &rooms_and_ids)
            .await
            .expect("Failed to mark sessions as backed up");
    }

    // Create a MemoryStore containing the supplied number of sessions.
    //
    // Sessions are returned in alphabetical order of session id.
    async fn store_with_sessions(
        num_sessions: usize,
        room_id: &RoomId,
    ) -> (MemoryStore, Vec<InboundGroupSession>) {
        let (account, _) = get_account_and_session_test_helper();

        let mut sessions = Vec::with_capacity(num_sessions);
        for _ in 0..num_sessions {
            sessions.push(new_session(&account, room_id).await);
        }
        sessions.sort_by_key(|s| s.session_id().to_owned());

        let store = MemoryStore::new();
        store.save_inbound_group_sessions(sessions.clone(), None).await.unwrap();

        (store, sessions)
    }

    // Create a new InboundGroupSession
    async fn new_session(account: &Account, room_id: &RoomId) -> InboundGroupSession {
        let curve_key = "Nn0L2hkcCMFKqynTjyGsJbth7QrVmX3lbrksMkrGOAw";
        let (outbound, _) = account.create_group_session_pair_with_defaults(room_id).await;

        InboundGroupSession::new(
            Curve25519PublicKey::from_base64(curve_key).unwrap(),
            Ed25519PublicKey::from_base64("ee3Ek+J2LkkPmjGPGLhMxiKnhiX//xcqaVL4RP6EypE").unwrap(),
            room_id,
            &outbound.session_key().await,
            SenderData::unknown(),
            None,
            outbound.settings().algorithm.to_owned(),
            None,
            false,
        )
        .unwrap()
    }

    /// Find the session_id and backed_up_to value for each of the sessions in
    /// the store.
    async fn backed_up_tos(store: &MemoryStore) -> HashMap<SessionId, String> {
        store
            .get_inbound_group_sessions_and_backed_up_to()
            .await
            .expect("Unable to get inbound group sessions and backup order")
            .iter()
            .map(|(s, o)| {
                (
                    s.session_id().to_owned(),
                    o.as_ref().map(|v| v.as_str().to_owned()).unwrap_or("".to_owned()),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod integration_tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex, OnceLock},
    };

    use async_trait::async_trait;
    use matrix_sdk_common::cross_process_lock::CrossProcessLockGeneration;
    use ruma::{
        DeviceId, OwnedDeviceId, RoomId, TransactionId, UserId, events::secret::request::SecretName,
    };
    use vodozemac::Curve25519PublicKey;

    use super::MemoryStore;
    use crate::{
        Account, DeviceData, GossipRequest, GossippedSecret, SecretInfo, Session, UserIdentityData,
        cryptostore_integration_tests, cryptostore_integration_tests_time,
        olm::{
            InboundGroupSession, OlmMessageHash, OutboundGroupSession, PrivateCrossSigningIdentity,
            SenderDataType, StaticAccountData,
        },
        store::{
            CryptoStore,
            types::{
                BackupKeys, Changes, DehydratedDeviceKey, PendingChanges, RoomKeyCounts,
                RoomKeyWithheldEntry, RoomSettings, StoredRoomKeyBundleData, TrackedUser,
            },
        },
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

    /// Return a clone of the store for the test with the supplied name. Note:
    /// dropping this store won't destroy its data, since
    /// [PersistentMemoryStore] is a reference-counted smart pointer
    /// to an underlying [MemoryStore].
    async fn get_store(
        name: &str,
        _passphrase: Option<&str>,
        clear_data: bool,
    ) -> PersistentMemoryStore {
        // Holds on to one [PersistentMemoryStore] per test, so even if the test drops
        // the store, we keep its data alive. This simulates the behaviour of
        // the other stores, which keep their data in a real DB, allowing us to
        // test MemoryStore using the same code.
        static STORES: OnceLock<Mutex<HashMap<String, PersistentMemoryStore>>> = OnceLock::new();
        let stores = STORES.get_or_init(|| Mutex::new(HashMap::new()));

        let mut stores = stores.lock().unwrap();

        if clear_data {
            // Create a new PersistentMemoryStore
            let new_store = PersistentMemoryStore::new();
            stores.insert(name.to_owned(), new_store.clone());
            new_store
        } else {
            stores.entry(name.to_owned()).or_insert_with(PersistentMemoryStore::new).clone()
        }
    }

    /// Forwards all methods to the underlying [MemoryStore].
    #[cfg_attr(target_family = "wasm", async_trait(?Send))]
    #[cfg_attr(not(target_family = "wasm"), async_trait)]
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

        async fn save_inbound_group_sessions(
            &self,
            sessions: Vec<InboundGroupSession>,
            backed_up_to_version: Option<&str>,
        ) -> Result<(), Self::Error> {
            self.0.save_inbound_group_sessions(sessions, backed_up_to_version).await
        }

        async fn get_sessions(
            &self,
            sender_key: &str,
        ) -> Result<Option<Vec<Session>>, Self::Error> {
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
        ) -> Result<Option<RoomKeyWithheldEntry>, Self::Error> {
            self.0.get_withheld_info(room_id, session_id).await
        }

        async fn get_withheld_sessions_by_room_id(
            &self,
            room_id: &RoomId,
        ) -> Result<Vec<RoomKeyWithheldEntry>, Self::Error> {
            self.0.get_withheld_sessions_by_room_id(room_id).await
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

        async fn get_inbound_group_sessions_by_room_id(
            &self,
            room_id: &RoomId,
        ) -> Result<Vec<InboundGroupSession>, Self::Error> {
            self.0.get_inbound_group_sessions_by_room_id(room_id).await
        }

        async fn get_inbound_group_sessions_for_device_batch(
            &self,
            sender_key: Curve25519PublicKey,
            sender_data_type: SenderDataType,
            after_session_id: Option<String>,
            limit: usize,
        ) -> Result<Vec<InboundGroupSession>, Self::Error> {
            self.0
                .get_inbound_group_sessions_for_device_batch(
                    sender_key,
                    sender_data_type,
                    after_session_id,
                    limit,
                )
                .await
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

        async fn load_dehydrated_device_pickle_key(
            &self,
        ) -> Result<Option<DehydratedDeviceKey>, Self::Error> {
            self.0.load_dehydrated_device_pickle_key().await
        }

        async fn delete_dehydrated_device_pickle_key(&self) -> Result<(), Self::Error> {
            self.0.delete_dehydrated_device_pickle_key().await
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
        ) -> Result<Option<DeviceData>, Self::Error> {
            self.0.get_device(user_id, device_id).await
        }

        async fn get_user_devices(
            &self,
            user_id: &UserId,
        ) -> Result<HashMap<OwnedDeviceId, DeviceData>, Self::Error> {
            self.0.get_user_devices(user_id).await
        }

        async fn get_own_device(&self) -> Result<DeviceData, Self::Error> {
            self.0.get_own_device().await
        }

        async fn get_user_identity(
            &self,
            user_id: &UserId,
        ) -> Result<Option<UserIdentityData>, Self::Error> {
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

        async fn get_received_room_key_bundle_data(
            &self,
            room_id: &RoomId,
            user_id: &UserId,
        ) -> crate::store::Result<Option<StoredRoomKeyBundleData>, Self::Error> {
            self.0.get_received_room_key_bundle_data(room_id, user_id).await
        }

        async fn has_downloaded_all_room_keys(
            &self,
            room_id: &RoomId,
        ) -> Result<bool, Self::Error> {
            self.0.has_downloaded_all_room_keys(room_id).await
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
        ) -> Result<Option<CrossProcessLockGeneration>, Self::Error> {
            self.0.try_take_leased_lock(lease_duration_ms, key, holder).await
        }

        async fn next_batch_token(&self) -> Result<Option<String>, Self::Error> {
            self.0.next_batch_token().await
        }

        async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
            self.0.get_size().await
        }
    }

    cryptostore_integration_tests!();
    cryptostore_integration_tests_time!();
}
