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
    sync::Arc,
};

use dashmap::DashMap;
use matrix_sdk_common::{
    api::r0::to_device::DeviceIdOrAllDevices,
    events::{room::encrypted::EncryptedEventContent, AnyMessageEventContent, EventType},
    identifiers::{DeviceId, DeviceIdBox, RoomId, UserId},
    uuid::Uuid,
};
use serde_json::Value;
use tracing::{debug, info};

use crate::{
    error::{EventError, MegolmResult, OlmResult},
    olm::{Account, InboundGroupSession, OutboundGroupSession, Session},
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

        Ok(session.encrypt(content).await)
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
        #[allow(clippy::map_clone)]
        if let Some(s) = self.outbound_group_sessions.get(room_id).map(|s| s.clone()) {
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

    /// Given a list of user and an outbound session get the list of users and
    /// devices that this session should be shared with.
    ///
    /// Returns a boolean indicating that the session needs to be rotated and
    /// the list of users/devices that should receive the session.
    pub async fn collect_session_recipients(
        &self,
        users: impl Iterator<Item = &UserId>,
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

        let mut device_got_deleted = false;

        for user_id in users {
            let user_devices = self.store.get_user_devices(&user_id).await?;

            // If no device got deleted until now and no user left check if one
            // got deleted for this user.
            if !device_got_deleted && !user_left {
                let device_ids: HashSet<&DeviceId> =
                    user_devices.keys().map(|d| d.as_ref()).collect();

                device_got_deleted = if let Some(shared) = outbound.shared_with_set.get(user_id) {
                    #[allow(clippy::map_clone)]
                    let shared: HashSet<DeviceIdBox> = shared.iter().map(|d| d.clone()).collect();
                    let shared: HashSet<&DeviceId> = shared.iter().map(|d| d.as_ref()).collect();
                    !shared
                        .difference(&device_ids)
                        .collect::<HashSet<_>>()
                        .is_empty()
                } else {
                    false
                };
            }

            devices
                .entry(user_id.clone())
                .or_insert_with(Vec::new)
                .extend(user_devices.devices().filter(|d| !d.is_blacklisted()));
        }

        // To protect the room history we need to rotate the session if a user
        // left or if a device got deleted, put differently if someone leaves
        // the encrypted group.
        let should_rotate = user_left || device_got_deleted;

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
        let mut changes = Changes::default();

        let (outbound, inbound) = self
            .get_or_create_outbound_session(room_id, encryption_settings.clone())
            .await?;

        if let Some(inbound) = inbound {
            changes.inbound_group_sessions.push(inbound);
        }

        let (should_rotate, devices) = self.collect_session_recipients(users, &outbound).await?;

        let outbound = if should_rotate {
            let (outbound, inbound) = self
                .create_outbound_group_session(room_id, encryption_settings)
                .await?;
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
                d.into_iter()
                    .filter(|d| !outbound.is_shared_with(d.user_id(), d.device_id()))
            })
            .flatten()
            .collect();

        if !devices.is_empty() {
            info!(
                "Sharing outbound session at index {} with {:?}",
                outbound.message_index().await,
                devices.iter().fold(HashMap::new(), |mut acc, d| {
                    acc.entry(d.user_id())
                        .or_insert_with(Vec::new)
                        .push(d.device_id());
                    acc
                })
            );
        }

        let key_content = outbound.as_json().await;

        for device_map_chunk in devices.chunks(Self::MAX_TO_DEVICE_MESSAGES) {
            let (id, request, used_sessions) = self
                .encrypt_session_for(key_content.clone(), device_map_chunk)
                .await?;

            outbound.add_request(id, request.into());
            changes.sessions.extend(used_sessions);
            self.outbound_sessions_being_shared
                .insert(id, outbound.clone());
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
