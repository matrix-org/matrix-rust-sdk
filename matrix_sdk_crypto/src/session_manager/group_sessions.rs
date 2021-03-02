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
use matrix_sdk_common::{
    api::r0::to_device::DeviceIdOrAllDevices,
    events::{
        room::{encrypted::EncryptedEventContent, history_visibility::HistoryVisibility},
        AnyMessageEventContent, EventType,
    },
    identifiers::{DeviceId, DeviceIdBox, RoomId, UserId},
    uuid::Uuid,
};
use serde_json::Value;
use tracing::{debug, info};

use crate::{
    error::{EventError, MegolmResult, OlmResult},
    olm::{Account, InboundGroupSession, OutboundGroupSession, Session, ShareState},
    store::{Changes, Store},
    Device, EncryptionSettings, OlmError, ToDeviceRequest,
};

#[derive(Debug, Clone)]
pub struct GroupSessionManager {
    account: Account,
    /// Store for the encryption keys.
    /// Persists all the encryption keys so a client can resume the session
    /// without the need to create new keys.
    store: Store,
    /// The currently active outbound group sessions.
    outbound_group_sessions: Arc<DashMap<RoomId, OutboundGroupSession>>,
    /// A map from the request id to the group session that the request belongs
    /// to. Used to mark requests belonging to the session as shared.
    outbound_sessions_being_shared: Arc<DashMap<Uuid, OutboundGroupSession>>,
}

impl GroupSessionManager {
    const MAX_TO_DEVICE_MESSAGES: usize = 20;

    pub(crate) fn new(account: Account, store: Store) -> Self {
        Self {
            account,
            store,
            outbound_group_sessions: Arc::new(DashMap::new()),
            outbound_sessions_being_shared: Arc::new(DashMap::new()),
        }
    }

    pub fn invalidate_group_session(&self, room_id: &RoomId) -> bool {
        if let Some(s) = self.outbound_group_sessions.get(room_id) {
            s.invalidate_session();
            true
        } else {
            false
        }
    }

    pub fn mark_request_as_sent(&self, request_id: &Uuid) {
        if let Some((_, s)) = self.outbound_sessions_being_shared.remove(request_id) {
            s.mark_request_as_sent(request_id);
        }
    }

    /// Get an outbound group session for a room, if one exists.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room for which we should get the outbound
    /// group session.
    pub fn get_outbound_group_session(&self, room_id: &RoomId) -> Option<OutboundGroupSession> {
        #[allow(clippy::map_clone)]
        self.outbound_group_sessions.get(room_id).map(|s| s.clone())
    }

    pub async fn encrypt(
        &self,
        room_id: &RoomId,
        content: AnyMessageEventContent,
    ) -> MegolmResult<EncryptedEventContent> {
        let session = if let Some(s) = self.get_outbound_group_session(room_id) {
            s
        } else {
            panic!("Session wasn't created nor shared");
        };

        if session.expired() {
            panic!("Session expired");
        }

        let content = session.encrypt(content).await;

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

        let _ = self
            .outbound_group_sessions
            .insert(room_id.to_owned(), outbound.clone());
        Ok((outbound, inbound))
    }

    pub async fn get_or_create_outbound_session(
        &self,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> OlmResult<(OutboundGroupSession, Option<InboundGroupSession>)> {
        // Get the cached session, if there isn't one load one from the store
        // and put it in the cache.
        let outbound_session = if let Some(s) = self.outbound_group_sessions.get(room_id) {
            Some(s.clone())
        } else if let Some(s) = self.store.get_outbound_group_sessions(room_id).await? {
            self.outbound_group_sessions
                .insert(room_id.clone(), s.clone());

            Some(s)
        } else {
            None
        };

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
            self.create_outbound_group_session(room_id, settings)
                .await
                .map(|(o, i)| (o, i.into()))
        }
    }

    /// Encrypt the given content for the given devices and create a to-device
    /// requests that sends the encrypted content to them.
    async fn encrypt_session_for(
        &self,
        content: Value,
        devices: &[Device],
    ) -> OlmResult<(Uuid, ToDeviceRequest, Vec<Session>)> {
        let mut messages = BTreeMap::new();
        let mut changed_sessions = Vec::new();

        for device in devices {
            let encrypted = device.encrypt(EventType::RoomKey, content.clone()).await;

            let (used_session, encrypted) = match encrypted {
                Ok(c) => c,
                Err(OlmError::MissingSession)
                | Err(OlmError::EventError(EventError::MissingSenderKey)) => {
                    continue;
                }
                Err(e) => return Err(e),
            };

            changed_sessions.push(used_session);

            messages
                .entry(device.user_id().clone())
                .or_insert_with(BTreeMap::new)
                .insert(
                    DeviceIdOrAllDevices::DeviceId(device.device_id().into()),
                    serde_json::value::to_raw_value(&encrypted)?,
                );
        }

        let id = Uuid::new_v4();

        let request = ToDeviceRequest {
            event_type: EventType::RoomEncrypted,
            txn_id: id,
            messages,
        };

        Ok((id, request, changed_sessions))
    }

    /// Given a list of user and an outbound session, return the list of users and
    /// their devices that this session should be shared with.
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

        let users_shared_with: HashSet<UserId> = outbound
            .shared_with_set
            .iter()
            .map(|k| k.key().clone())
            .collect();

        let users_shared_with: HashSet<&UserId> = users_shared_with.iter().collect();

        // A user left if a user is missing from the set of users that should
        // get the session but is in the set of users that received the sessoin.
        let user_left = !users_shared_with
            .difference(&users)
            .collect::<HashSet<_>>()
            .is_empty();

        let visiblity_changed = outbound.settings().history_visibility != history_visibility;

        // To protect the room history we need to rotate the session if either:
        //
        // 1. Any user left the room.
        // 2. Any of the users' devices got deleted or blacklisted.
        // 3. The history visibility changed.
        //
        // This is calculated in the following code and stored in this variable.
        let mut should_rotate = user_left || visiblity_changed;

        for user_id in users {
            let user_devices = self.store.get_user_devices(&user_id).await?;

            // If we haven't already concluded that the session should be rotated, check whether
            // any of the devices in the session got deleted or blacklisted in the meantime. If so,
            // we should also rotate the session.
            if !should_rotate {
                // Devices that should receive this session
                let device_ids: HashSet<&DeviceId> = user_devices
                    .keys()
                    .filter(|d| {
                        !user_devices
                            .get(d)
                            .map_or(false, |d| d.is_blacklisted())
                    })
                    .map(|d| d.as_ref())
                    .collect();

                if let Some(shared) = outbound.shared_with_set.get(user_id) {
                    #[allow(clippy::map_clone)]
                    // Devices that received this session
                    let shared: HashSet<DeviceIdBox> =
                        shared.iter().map(|d| d.key().clone()).collect();
                    let shared: HashSet<&DeviceId> = shared.iter().map(|d| d.as_ref()).collect();

                    // The difference between the devices that received the
                    // session and devices that should receive the session are
                    // our deleted or newly blacklisted devices
                    //
                    // If the set isn't empty, a device got blacklisted or
                    // deleted, so remember it.
                    if !shared
                        .difference(&device_ids)
                        .collect::<HashSet<_>>()
                        .is_empty()
                    {
                        should_rotate = true;
                    }
                };
            }

            devices
                .entry(user_id.clone())
                .or_insert_with(Vec::new)
                .extend(user_devices.devices().filter(|d| !d.is_blacklisted()));
        }

        Ok((should_rotate, devices))
    }

    /// Get to-device requests to share a group session with users in a room.
    ///
    /// # Arguments
    ///
    /// `room_id` - The room id of the room where the group session will be
    /// used.
    ///
    /// `users` - The list of users that should receive the group session.
    pub async fn share_group_session(
        &self,
        room_id: &RoomId,
        users: impl Iterator<Item = &UserId>,
        encryption_settings: impl Into<EncryptionSettings>,
    ) -> OlmResult<Vec<Arc<ToDeviceRequest>>> {
        let encryption_settings = encryption_settings.into();
        let history_visibility = encryption_settings.history_visibility.clone();
        let mut changes = Changes::default();

        let (outbound, inbound) = self
            .get_or_create_outbound_session(room_id, encryption_settings.clone())
            .await?;

        if let Some(inbound) = inbound {
            changes.outbound_group_sessions.push(outbound.clone());
            changes.inbound_group_sessions.push(inbound);
        }

        let (should_rotate, devices) = self
            .collect_session_recipients(users, history_visibility, &outbound)
            .await?;

        let outbound = if should_rotate {
            let (outbound, inbound) = self
                .create_outbound_group_session(room_id, encryption_settings)
                .await?;
            changes.outbound_group_sessions.push(outbound.clone());
            changes.inbound_group_sessions.push(inbound);

            debug!(
                "A user/device has left the group {} since we last sent a message, \
                   rotating the outbound session.",
                room_id
            );

            outbound
        } else {
            outbound
        };

        let devices: Vec<Device> = devices
            .into_iter()
            .map(|(_, d)| {
                d.into_iter().filter(|d| {
                    matches!(
                        outbound.is_shared_with(d.user_id(), d.device_id()),
                        ShareState::NotShared
                    )
                })
            })
            .flatten()
            .collect();

        let key_content = outbound.as_json().await;
        let message_index = outbound.message_index().await;

        if !devices.is_empty() {
            info!(
                "Sharing outbound session at index {} with {:?}",
                message_index,
                devices.iter().fold(BTreeMap::new(), |mut acc, d| {
                    acc.entry(d.user_id())
                        .or_insert_with(BTreeSet::new)
                        .insert(d.device_id());
                    acc
                })
            );
        }

        for device_map_chunk in devices.chunks(Self::MAX_TO_DEVICE_MESSAGES) {
            let (id, request, used_sessions) = self
                .encrypt_session_for(key_content.clone(), device_map_chunk)
                .await?;

            if !request.messages.is_empty() {
                outbound.add_request(id, request.into(), message_index);
                self.outbound_sessions_being_shared
                    .insert(id, outbound.clone());
            }

            changes.sessions.extend(used_sessions);
        }

        let requests = outbound.pending_requests();

        if requests.is_empty() {
            debug!(
                "Session {} for room {} doesn't need to be shared with anyone, marking as shared",
                outbound.session_id(),
                outbound.room_id()
            );
            outbound.mark_as_shared();
        }

        self.store.save_changes(changes).await?;

        Ok(requests)
    }
}
