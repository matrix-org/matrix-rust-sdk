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
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

use dashmap::DashMap;
use futures::future::join_all;
use matrix_sdk_common::{executor::spawn, uuid::Uuid};
use ruma::{
    events::{
        room::{encrypted::EncryptedEventContent, history_visibility::HistoryVisibility},
        AnyToDeviceEventContent, EventType,
    },
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
    DeviceId, DeviceIdBox, RoomId, UserId,
};
use serde_json::Value;
use tracing::{debug, info, trace};

use crate::{
    error::{EventError, MegolmResult, OlmResult},
    olm::{Account, InboundGroupSession, OutboundGroupSession, Session, ShareInfo, ShareState},
    store::{Changes, Result as StoreResult, Store},
    Device, EncryptionSettings, OlmError, ToDeviceRequest,
};

#[derive(Clone, Debug)]
pub(crate) struct GroupSessionCache {
    store: Store,
    sessions: Arc<DashMap<RoomId, OutboundGroupSession>>,
    /// A map from the request id to the group session that the request belongs
    /// to. Used to mark requests belonging to the session as shared.
    sessions_being_shared: Arc<DashMap<Uuid, OutboundGroupSession>>,
}

impl GroupSessionCache {
    pub(crate) fn new(store: Store) -> Self {
        Self { store, sessions: Default::default(), sessions_being_shared: Default::default() }
    }

    pub(crate) fn insert(&self, session: OutboundGroupSession) {
        self.sessions.insert(session.room_id().to_owned(), session);
    }

    /// Either get a session for the given room from the cache or load it from
    /// the store.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room this session is used for.
    pub async fn get_or_load(&self, room_id: &RoomId) -> StoreResult<Option<OutboundGroupSession>> {
        // Get the cached session, if there isn't one load one from the store
        // and put it in the cache.
        if let Some(s) = self.sessions.get(room_id) {
            Ok(Some(s.clone()))
        } else if let Some(s) = self.store.get_outbound_group_sessions(room_id).await? {
            for request_id in s.pending_request_ids() {
                self.sessions_being_shared.insert(request_id, s.clone());
            }

            self.sessions.insert(room_id.clone(), s.clone());

            Ok(Some(s))
        } else {
            Ok(None)
        }
    }

    /// Get an outbound group session for a room, if one exists.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which we should get the outbound
    /// group session.
    fn get(&self, room_id: &RoomId) -> Option<OutboundGroupSession> {
        self.sessions.get(room_id).map(|s| s.clone())
    }

    /// Get or load the session for the given room with the given session id.
    ///
    /// This is the same as [get_or_load()](#method.get_or_load) but it will
    /// filter out the session if it doesn't match the given session id.
    pub async fn get_with_id(
        &self,
        room_id: &RoomId,
        session_id: &str,
    ) -> StoreResult<Option<OutboundGroupSession>> {
        Ok(self.get_or_load(room_id).await?.filter(|o| session_id == o.session_id()))
    }
}

#[derive(Debug, Clone)]
pub struct GroupSessionManager {
    account: Account,
    /// Store for the encryption keys.
    /// Persists all the encryption keys so a client can resume the session
    /// without the need to create new keys.
    store: Store,
    /// The currently active outbound group sessions.
    sessions: GroupSessionCache,
}

impl GroupSessionManager {
    const MAX_TO_DEVICE_MESSAGES: usize = 250;

    pub(crate) fn new(account: Account, store: Store) -> Self {
        Self { account, store: store.clone(), sessions: GroupSessionCache::new(store) }
    }

    pub async fn invalidate_group_session(&self, room_id: &RoomId) -> StoreResult<bool> {
        if let Some(s) = self.sessions.get(room_id) {
            s.invalidate_session();

            let mut changes = Changes::default();
            changes.outbound_group_sessions.push(s.clone());
            self.store.save_changes(changes).await?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn mark_request_as_sent(&self, request_id: &Uuid) -> StoreResult<()> {
        if let Some((_, s)) = self.sessions.sessions_being_shared.remove(request_id) {
            s.mark_request_as_sent(request_id);

            let mut changes = Changes::default();
            changes.outbound_group_sessions.push(s.clone());
            self.store.save_changes(changes).await?;
        }

        Ok(())
    }

    #[cfg(test)]
    pub fn get_outbound_group_session(&self, room_id: &RoomId) -> Option<OutboundGroupSession> {
        self.sessions.get(room_id)
    }

    pub async fn encrypt(
        &self,
        room_id: &RoomId,
        content: Value,
        event_type: &str,
    ) -> MegolmResult<EncryptedEventContent> {
        let session = if let Some(s) = self.sessions.get(room_id) {
            s
        } else {
            panic!("Session wasn't created nor shared");
        };

        assert!(!session.expired(), "Session expired");

        let content = session.encrypt(content, event_type).await;

        let mut changes = Changes::default();
        changes.outbound_group_sessions.push(session);
        self.store.save_changes(changes).await?;

        Ok(content)
    }

    /// Create a new outbound group session.
    ///
    /// This also creates a matching inbound group session and saves that one in
    /// the store.
    pub async fn create_outbound_group_session(
        &self,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> OlmResult<(OutboundGroupSession, InboundGroupSession)> {
        let (outbound, inbound) = self
            .account
            .create_group_session_pair(room_id, settings)
            .await
            .map_err(|_| EventError::UnsupportedAlgorithm)?;

        self.sessions.insert(outbound.clone());
        Ok((outbound, inbound))
    }

    pub async fn get_or_create_outbound_session(
        &self,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> OlmResult<(OutboundGroupSession, Option<InboundGroupSession>)> {
        let outbound_session = self.sessions.get_or_load(room_id).await?;

        // If there is no session or the session has expired or is invalid,
        // create a new one.
        if let Some(s) = outbound_session {
            if s.expired() || s.invalidated() {
                self.create_outbound_group_session(room_id, settings)
                    .await
                    .map(|(o, i)| (o, i.into()))
            } else {
                Ok((s, None))
            }
        } else {
            self.create_outbound_group_session(room_id, settings).await.map(|(o, i)| (o, i.into()))
        }
    }

    /// Encrypt the given content for the given devices and create a to-device
    /// requests that sends the encrypted content to them.
    async fn encrypt_session_for(
        content: AnyToDeviceEventContent,
        devices: Vec<Device>,
        message_index: u32,
    ) -> OlmResult<(
        Uuid,
        ToDeviceRequest,
        BTreeMap<UserId, BTreeMap<DeviceIdBox, ShareInfo>>,
        Vec<Session>,
    )> {
        let mut messages = BTreeMap::new();
        let mut changed_sessions = Vec::new();
        let mut share_infos = BTreeMap::new();

        let encrypt = |device: Device, content: AnyToDeviceEventContent| async move {
            let mut message = BTreeMap::new();
            let mut share_infos = BTreeMap::new();

            let encrypted = device.encrypt(content.clone()).await;

            let used_session = match encrypted {
                Ok((session, encrypted)) => {
                    message
                        .entry(device.user_id().to_owned())
                        .or_insert_with(BTreeMap::new)
                        .insert(
                            DeviceIdOrAllDevices::DeviceId(device.device_id().into()),
                            Raw::from(AnyToDeviceEventContent::RoomEncrypted(encrypted)),
                        );
                    share_infos
                        .entry(device.user_id().to_owned())
                        .or_insert_with(BTreeMap::new)
                        .insert(
                            device.device_id().to_owned(),
                            ShareInfo {
                                sender_key: session.sender_key().to_owned(),
                                message_index,
                            },
                        );

                    Some(session)
                }
                // TODO we'll want to create m.room_key.withheld here.
                Err(OlmError::MissingSession)
                | Err(OlmError::EventError(EventError::MissingSenderKey)) => None,
                Err(e) => return Err(e),
            };

            Ok((used_session, share_infos, message))
        };

        let tasks: Vec<_> =
            devices.iter().map(|d| spawn(encrypt(d.clone(), content.clone()))).collect();

        let results = join_all(tasks).await;

        for result in results {
            let (used_session, infos, message) = result.expect("Encryption task panicked")?;

            if let Some(session) = used_session {
                changed_sessions.push(session);
            }

            for (user, device_messages) in message.into_iter() {
                messages.entry(user).or_insert_with(BTreeMap::new).extend(device_messages);
            }

            for (user, infos) in infos.into_iter() {
                share_infos.entry(user).or_insert_with(BTreeMap::new).extend(infos);
            }
        }

        let id = Uuid::new_v4();

        let request =
            ToDeviceRequest { event_type: EventType::RoomEncrypted, txn_id: id, messages };

        trace!(
            recipient_count = request.message_count(),
            transaction_id = ?id,
            "Created a to-device request carrying a room_key"
        );

        Ok((id, request, share_infos, changed_sessions))
    }

    /// Given a list of user and an outbound session, return the list of users
    /// and their devices that this session should be shared with.
    ///
    /// Returns a boolean indicating whether the session needs to be rotated and
    /// the list of users/devices that should receive the session.
    pub async fn collect_session_recipients(
        &self,
        users: impl Iterator<Item = &UserId>,
        history_visibility: HistoryVisibility,
        outbound: &OutboundGroupSession,
    ) -> OlmResult<(bool, HashMap<UserId, Vec<Device>>)> {
        let users: HashSet<&UserId> = users.collect();
        let mut devices: HashMap<UserId, Vec<Device>> = HashMap::new();

        debug!(
            users = ?users,
            history_visibility = ?history_visibility,
            session_id = outbound.session_id(),
            "Calculating group session recipients"
        );

        let users_shared_with: HashSet<UserId> =
            outbound.shared_with_set.iter().map(|k| k.key().clone()).collect();

        let users_shared_with: HashSet<&UserId> = users_shared_with.iter().collect();

        // A user left if a user is missing from the set of users that should
        // get the session but is in the set of users that received the session.
        let user_left = !users_shared_with.difference(&users).collect::<HashSet<_>>().is_empty();

        let visibility_changed = outbound.settings().history_visibility != history_visibility;

        // To protect the room history we need to rotate the session if either:
        //
        // 1. Any user left the room.
        // 2. Any of the users' devices got deleted or blacklisted.
        // 3. The history visibility changed.
        //
        // This is calculated in the following code and stored in this variable.
        let mut should_rotate = user_left || visibility_changed;

        for user_id in users {
            let user_devices = self.store.get_user_devices(user_id).await?;
            let non_blacklisted_devices: Vec<Device> =
                user_devices.devices().filter(|d| !d.is_blacklisted()).collect();

            // If we haven't already concluded that the session should be
            // rotated for other reasons, we also need to check whether any
            // of the devices in the session got deleted or blacklisted in the
            // meantime. If so, we should also rotate the session.
            if !should_rotate {
                // Device IDs that should receive this session
                let non_blacklisted_device_ids: HashSet<&DeviceId> =
                    non_blacklisted_devices.iter().map(|d| d.device_id()).collect();

                if let Some(shared) = outbound.shared_with_set.get(user_id) {
                    // Devices that received this session
                    let shared: HashSet<DeviceIdBox> =
                        shared.iter().map(|d| d.key().clone()).collect();
                    let shared: HashSet<&DeviceId> = shared.iter().map(|d| d.as_ref()).collect();

                    // The set difference between
                    //
                    // 1. Devices that had previously received the session, and
                    // 2. Devices that would now receive the session
                    //
                    // represents newly deleted or blacklisted devices. If this
                    // set is non-empty, we must rotate.
                    let newly_deleted_or_blacklisted =
                        shared.difference(&non_blacklisted_device_ids).collect::<HashSet<_>>();

                    if !newly_deleted_or_blacklisted.is_empty() {
                        should_rotate = true;
                    }
                };
            }

            devices.entry(user_id.clone()).or_insert_with(Vec::new).extend(non_blacklisted_devices);
        }

        debug!(
            should_rotate = should_rotate,
            session_id = outbound.session_id(),
            "Done calculating group session recipients"
        );

        Ok((should_rotate, devices))
    }

    pub async fn encrypt_request(
        chunk: Vec<Device>,
        content: AnyToDeviceEventContent,
        outbound: OutboundGroupSession,
        message_index: u32,
        being_shared: Arc<DashMap<Uuid, OutboundGroupSession>>,
    ) -> OlmResult<Vec<Session>> {
        let (id, request, share_infos, used_sessions) =
            Self::encrypt_session_for(content.clone(), chunk, message_index).await?;

        if !request.messages.is_empty() {
            outbound.add_request(id, request.into(), share_infos);
            being_shared.insert(id, outbound.clone());
        }

        Ok(used_sessions)
    }

    pub(crate) fn session_cache(&self) -> GroupSessionCache {
        self.sessions.clone()
    }

    /// Get to-device requests to share a group session with users in a room.
    ///
    /// # Arguments
    ///
    /// `room_id` - The room id of the room where the group session will be
    /// used.
    ///
    /// `users` - The list of users that should receive the group session.
    ///
    /// `encryption_settings` - The settings that should be used for the group
    /// session.
    pub async fn share_group_session(
        &self,
        room_id: &RoomId,
        users: impl Iterator<Item = &UserId>,
        encryption_settings: impl Into<EncryptionSettings>,
    ) -> OlmResult<Vec<Arc<ToDeviceRequest>>> {
        debug!(room_id = room_id.as_str(), "Checking if a room key needs to be shared",);

        let encryption_settings = encryption_settings.into();
        let history_visibility = encryption_settings.history_visibility.clone();
        let mut changes = Changes::default();

        let (outbound, inbound) =
            self.get_or_create_outbound_session(room_id, encryption_settings.clone()).await?;

        if let Some(inbound) = inbound {
            changes.outbound_group_sessions.push(outbound.clone());
            changes.inbound_group_sessions.push(inbound);
        }

        let (should_rotate, devices) =
            self.collect_session_recipients(users, history_visibility, &outbound).await?;

        let outbound = if should_rotate {
            let old_session_id = outbound.session_id();

            let (outbound, inbound) =
                self.create_outbound_group_session(room_id, encryption_settings).await?;
            changes.outbound_group_sessions.push(outbound.clone());
            changes.inbound_group_sessions.push(inbound);

            debug!(
                room_id = room_id.as_str(),
                old_session_id = old_session_id,
                session_id = outbound.session_id(),
                "A user/device has left the group since we last sent a message, \
                   rotating the outbound session.",
            );

            outbound
        } else {
            outbound
        };

        let devices: Vec<Device> = devices
            .into_iter()
            .map(|(_, d)| {
                d.into_iter()
                    .filter(|d| matches!(outbound.is_shared_with(d), ShareState::NotShared))
            })
            .flatten()
            .collect();

        let key_content = outbound.as_content().await;
        let message_index = outbound.message_index().await;

        if !devices.is_empty() {
            let users = devices.iter().fold(BTreeMap::new(), |mut acc, d| {
                acc.entry(d.user_id()).or_insert_with(BTreeSet::new).insert(d.device_id());
                acc
            });

            info!(
                index = message_index,
                users = ?users,
                room_id = room_id.as_str(),
                "Sharing an outbound group session",
            );
        }

        let tasks: Vec<_> = devices
            .chunks(Self::MAX_TO_DEVICE_MESSAGES)
            .map(|chunk| {
                spawn(Self::encrypt_request(
                    chunk.to_vec(),
                    key_content.clone(),
                    outbound.clone(),
                    message_index,
                    self.sessions.sessions_being_shared.clone(),
                ))
            })
            .collect();

        for result in join_all(tasks).await {
            let used_sessions: OlmResult<Vec<Session>> = result.expect("Encryption task panicked");

            changes.sessions.extend(used_sessions?);
        }

        let requests = outbound.pending_requests();

        debug!(
            room_id = room_id.as_str(),
            session_id = outbound.session_id(),
            request_count = requests.len(),
            "Done generating to-device requests for a room key share"
        );

        if requests.is_empty() {
            debug!(
                room_id = room_id.as_str(),
                session_id = outbound.session_id(),
                "The outbound group session doesn't need to be shared with \
                    anyone, marking as shared",
            );
            outbound.mark_as_shared();
        }

        let session_count = changes.sessions.len();

        self.store.save_changes(changes).await?;

        debug!(
            room_id = room_id.as_str(),
            session_id = outbound.session_id(),
            session_count = session_count,
            "Stored the changed sessions after encrypting an room key"
        );

        Ok(requests)
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_common::uuid::Uuid;
    use matrix_sdk_test::response_from_file;
    use ruma::{
        api::{
            client::r0::keys::{claim_keys, get_keys},
            IncomingResponse,
        },
        room_id, user_id, DeviceIdBox, UserId,
    };
    use serde_json::Value;

    use crate::{EncryptionSettings, OlmMachine};

    fn alice_id() -> UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> DeviceIdBox {
        "JLAFKJWSCS".into()
    }

    fn keys_query_response() -> get_keys::Response {
        let data = include_bytes!("../../benches/keys_query.json");
        let data: Value = serde_json::from_slice(data).unwrap();
        let data = response_from_file(&data);
        get_keys::Response::try_from_http_response(data)
            .expect("Can't parse the keys upload response")
    }

    fn keys_claim_response() -> claim_keys::Response {
        let data = include_bytes!("../../benches/keys_claim.json");
        let data: Value = serde_json::from_slice(data).unwrap();
        let data = response_from_file(&data);
        claim_keys::Response::try_from_http_response(data)
            .expect("Can't parse the keys upload response")
    }

    async fn machine() -> OlmMachine {
        let keys_query = keys_query_response();
        let keys_claim = keys_claim_response();
        let uuid = Uuid::new_v4();

        let machine = OlmMachine::new(&alice_id(), &alice_device_id());

        machine.mark_request_as_sent(&uuid, &keys_query).await.unwrap();
        machine.mark_request_as_sent(&uuid, &keys_claim).await.unwrap();

        machine
    }

    #[tokio::test]
    async fn test_sharing() {
        let machine = machine().await;
        let room_id = room_id!("!test:localhost");
        let keys_claim = keys_claim_response();

        let users: Vec<_> = keys_claim.one_time_keys.keys().collect();

        let requests = machine
            .share_group_session(&room_id, users.clone().into_iter(), EncryptionSettings::default())
            .await
            .unwrap();

        let event_count: usize = requests.iter().map(|r| r.message_count()).sum();

        // The keys claim response has a couple of one-time keys with invalid
        // signatures, thus only 148 sessions are actually created, we check
        // that all 148 valid sessions get an room key.
        assert_eq!(event_count, 148);
    }
}
