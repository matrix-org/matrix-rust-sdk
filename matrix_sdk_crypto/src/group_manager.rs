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

use std::{collections::BTreeMap, sync::Arc};

use dashmap::DashMap;
use matrix_sdk_common::{
    api::r0::to_device::DeviceIdOrAllDevices,
    events::{room::encrypted::EncryptedEventContent, AnyMessageEventContent, EventType},
    identifiers::{RoomId, UserId},
    uuid::Uuid,
};

use crate::{
    error::{EventError, MegolmResult, OlmResult},
    key_request::Device,
    olm::{Account, OutboundGroupSession},
    store::{Result as StoreResult, Store},
    EncryptionSettings, OlmError, ToDeviceRequest,
};

#[derive(Clone)]
pub struct GroupSessionManager {
    account: Account,
    /// Store for the encryption keys.
    /// Persists all the encryption keys so a client can resume the session
    /// without the need to create new keys.
    store: Store,
    /// The currently active outbound group sessions.
    outbound_group_sessions: Arc<DashMap<RoomId, OutboundGroupSession>>,
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

    async fn get_user_devices(&self, user_id: &UserId) -> StoreResult<Vec<Device>> {
        let devices = self.store.get_user_devices(user_id).await?;

        let own_identity = self
            .store
            .get_user_identity(self.account.user_id())
            .await?
            .map(|i| i.own().cloned())
            .flatten();
        let device_owner_identity = self.store.get_user_identity(user_id).await.ok().flatten();

        Ok(devices
            .devices()
            .map(|d| Device {
                inner: d.clone(),
                store: self.store.clone(),
                own_identity: own_identity.clone(),
                device_owner_identity: device_owner_identity.clone(),
            })
            .collect())
    }

    pub fn invalidate_group_session(&self, room_id: &RoomId) -> bool {
        self.outbound_group_sessions.remove(room_id).is_some()
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

    /// Should the client share a group session for the given room.
    ///
    /// Returns true if a session needs to be shared before room messages can be
    /// encrypted, false if one is already shared and ready to encrypt room
    /// messages.
    ///
    /// This should be called every time a new room message wants to be sent out
    /// since group sessions can expire at any time.
    pub fn should_share_group_session(&self, room_id: &RoomId) -> bool {
        let session = self.outbound_group_sessions.get(room_id);

        match session {
            Some(s) => !s.shared() || s.expired(),
            None => true,
        }
    }

    /// Create a new outbound group session.
    ///
    /// This also creates a matching inbound group session and saves that one in
    /// the store.
    pub async fn create_outbound_group_session(
        &self,
        room_id: &RoomId,
        settings: EncryptionSettings,
        users_to_share_with: impl Iterator<Item = &UserId>,
    ) -> OlmResult<()> {
        let (outbound, inbound) = self
            .account
            .create_group_session_pair(room_id, settings, users_to_share_with)
            .await
            .map_err(|_| EventError::UnsupportedAlgorithm)?;

        let _ = self.store.save_inbound_group_sessions(&[inbound]).await?;

        let _ = self
            .outbound_group_sessions
            .insert(room_id.to_owned(), outbound);
        Ok(())
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
    ) -> OlmResult<Vec<ToDeviceRequest>> {
        self.create_outbound_group_session(room_id, encryption_settings.into(), users)
            .await?;
        let session = self.outbound_group_sessions.get(room_id).unwrap();

        if session.shared() {
            panic!("Session is already shared");
        }

        // TODO don't mark the session as shared automatically, only when all
        // the requests are done, failure to send these requests will likely end
        // up in wedged sessions. We'll need to store the requests and let the
        // caller mark them as sent using an UUID.
        session.mark_as_shared();

        let mut devices = Vec::new();

        for user_id in session.users_to_share_with() {
            let mut user_devices = self.get_user_devices(&user_id).await?;
            devices.extend(user_devices.drain(..).filter(|d| !d.is_blacklisted()))
        }

        let mut requests = Vec::new();
        let key_content = session.as_json().await;

        for device_map_chunk in devices.chunks(Self::MAX_TO_DEVICE_MESSAGES) {
            let mut messages = BTreeMap::new();

            for device in device_map_chunk {
                let encrypted = device
                    .encrypt(EventType::RoomKey, key_content.clone())
                    .await;

                let encrypted = match encrypted {
                    Ok(c) => c,
                    Err(OlmError::MissingSession)
                    | Err(OlmError::EventError(EventError::MissingSenderKey)) => {
                        continue;
                    }
                    Err(e) => return Err(e),
                };

                messages
                    .entry(device.user_id().clone())
                    .or_insert_with(BTreeMap::new)
                    .insert(
                        DeviceIdOrAllDevices::DeviceId(device.device_id().into()),
                        serde_json::value::to_raw_value(&encrypted)?,
                    );
            }

            let id = Uuid::new_v4();

            requests.push(ToDeviceRequest {
                event_type: EventType::RoomEncrypted,
                txn_id: id,
                messages,
            });
        }

        Ok(requests)
    }
}
