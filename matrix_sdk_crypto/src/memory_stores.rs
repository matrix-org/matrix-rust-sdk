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

use std::{collections::HashMap, sync::Arc};

use dashmap::{DashMap, ReadOnlyView};
use matrix_sdk_common::locks::Mutex;

use super::{
    device::Device,
    olm::{InboundGroupSession, Session},
};
use matrix_sdk_common::identifiers::{DeviceId, RoomId, UserId};

/// In-memory store for Olm Sessions.
#[derive(Debug, Default)]
pub struct SessionStore {
    entries: HashMap<String, Arc<Mutex<Vec<Session>>>>,
}

impl SessionStore {
    /// Create a new empty Session store.
    pub fn new() -> Self {
        SessionStore {
            entries: HashMap::new(),
        }
    }

    /// Add a session to the store.
    ///
    /// Returns true if the the session was added, false if the session was
    /// already in the store.
    pub async fn add(&mut self, session: Session) -> bool {
        if !self.entries.contains_key(&*session.sender_key) {
            self.entries.insert(
                session.sender_key.to_string(),
                Arc::new(Mutex::new(Vec::new())),
            );
        }
        let sessions = self.entries.get_mut(&*session.sender_key).unwrap();

        if !sessions.lock().await.contains(&session) {
            sessions.lock().await.push(session);
            true
        } else {
            false
        }
    }

    /// Get all the sessions that belong to the given sender key.
    pub fn get(&self, sender_key: &str) -> Option<Arc<Mutex<Vec<Session>>>> {
        self.entries.get(sender_key).cloned()
    }

    /// Add a list of sessions belonging to the sender key.
    pub fn set_for_sender(&mut self, sender_key: &str, sessions: Vec<Session>) {
        self.entries
            .insert(sender_key.to_owned(), Arc::new(Mutex::new(sessions)));
    }
}

#[derive(Debug, Default)]
/// In-memory store that houlds inbound group sessions.
pub struct GroupSessionStore {
    entries: HashMap<RoomId, HashMap<String, HashMap<String, InboundGroupSession>>>,
}

impl GroupSessionStore {
    /// Create a new empty store.
    pub fn new() -> Self {
        GroupSessionStore {
            entries: HashMap::new(),
        }
    }

    /// Add a inbound group session to the store.
    ///
    /// Returns true if the the session was added, false if the session was
    /// already in the store.
    pub fn add(&mut self, session: InboundGroupSession) -> bool {
        if !self.entries.contains_key(&session.room_id) {
            let room_id = &*session.room_id;
            self.entries.insert(room_id.clone(), HashMap::new());
        }

        let room_map = self.entries.get_mut(&session.room_id).unwrap();

        if !room_map.contains_key(&*session.sender_key) {
            let sender_key = &*session.sender_key;
            room_map.insert(sender_key.to_owned(), HashMap::new());
        }

        let sender_map = room_map.get_mut(&*session.sender_key).unwrap();
        let ret = sender_map.insert(session.session_id().to_owned(), session);

        ret.is_none()
    }

    /// Get a inbound group session from our store.
    ///
    /// # Arguments
    /// * `room_id` - The room id of the room that the session belongs to.
    ///
    /// * `sender_key` - The sender key that sent us the session.
    ///
    /// * `session_id` - The unique id of the session.
    pub fn get(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Option<InboundGroupSession> {
        self.entries
            .get(room_id)
            .and_then(|m| m.get(sender_key).and_then(|m| m.get(session_id).cloned()))
    }
}

/// In-memory store holding the devices of users.
#[derive(Clone, Debug, Default)]
pub struct DeviceStore {
    entries: Arc<DashMap<UserId, DashMap<Box<DeviceId>, Device>>>,
}

/// A read only view over all devices belonging to a user.
#[derive(Debug)]
pub struct UserDevices {
    entries: ReadOnlyView<Box<DeviceId>, Device>,
}

impl UserDevices {
    /// Get the specific device with the given device id.
    pub fn get(&self, device_id: &DeviceId) -> Option<Device> {
        self.entries.get(device_id).cloned()
    }

    /// Iterator over all the device ids of the user devices.
    pub fn keys(&self) -> impl Iterator<Item = &DeviceId> {
        self.entries.keys().map(|id| id.as_ref())
    }

    /// Iterator over all the devices of the user devices.
    pub fn devices(&self) -> impl Iterator<Item = &Device> {
        self.entries.values()
    }
}

impl DeviceStore {
    /// Create a new empty device store.
    pub fn new() -> Self {
        DeviceStore {
            entries: Arc::new(DashMap::new()),
        }
    }

    /// Add a device to the store.
    ///
    /// Returns true if the device was already in the store, false otherwise.
    pub fn add(&self, device: Device) -> bool {
        let user_id = device.user_id();

        if !self.entries.contains_key(&user_id) {
            self.entries.insert(user_id.clone(), DashMap::new());
        }
        let device_map = self.entries.get_mut(&user_id).unwrap();

        device_map
            .insert(device.device_id().into(), device)
            .is_none()
    }

    /// Get the device with the given device_id and belonging to the given user.
    pub fn get(&self, user_id: &UserId, device_id: &DeviceId) -> Option<Device> {
        self.entries
            .get(user_id)
            .and_then(|m| m.get(device_id).map(|d| d.value().clone()))
    }

    /// Remove the device with the given device_id and belonging to the given user.
    ///
    /// Returns the device if it was removed, None if it wasn't in the store.
    pub fn remove(&self, user_id: &UserId, device_id: &DeviceId) -> Option<Device> {
        self.entries
            .get(user_id)
            .and_then(|m| m.remove(device_id))
            .map(|(_, d)| d)
    }

    /// Get a read-only view over all devices of the given user.
    pub fn user_devices(&self, user_id: &UserId) -> UserDevices {
        if !self.entries.contains_key(user_id) {
            self.entries.insert(user_id.clone(), DashMap::new());
        }
        UserDevices {
            entries: self.entries.get(user_id).unwrap().clone().into_read_only(),
        }
    }
}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use crate::{
        device::test::get_device,
        memory_stores::{DeviceStore, GroupSessionStore, SessionStore},
        olm::{test::get_account_and_session, InboundGroupSession},
    };
    use matrix_sdk_common::identifiers::RoomId;

    #[tokio::test]
    async fn test_session_store() {
        let (_, session) = get_account_and_session().await;

        let mut store = SessionStore::new();

        assert!(store.add(session.clone()).await);
        assert!(!store.add(session.clone()).await);

        let sessions = store.get(&session.sender_key).unwrap();
        let sessions = sessions.lock().await;

        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[tokio::test]
    async fn test_session_store_bulk_storing() {
        let (_, session) = get_account_and_session().await;

        let mut store = SessionStore::new();
        store.set_for_sender(&session.sender_key, vec![session.clone()]);

        let sessions = store.get(&session.sender_key).unwrap();
        let sessions = sessions.lock().await;

        let loaded_session = &sessions[0];

        assert_eq!(&session, loaded_session);
    }

    #[tokio::test]
    async fn test_group_session_store() {
        let (account, _) = get_account_and_session().await;
        let room_id = RoomId::try_from("!test:localhost").unwrap();

        let (outbound, _) = account.create_group_session_pair(&room_id).await;

        assert_eq!(0, outbound.message_index().await);
        assert!(!outbound.shared());
        outbound.mark_as_shared();
        assert!(outbound.shared());

        let inbound = InboundGroupSession::new(
            "test_key",
            "test_key",
            &room_id,
            outbound.session_key().await,
        )
        .unwrap();

        let mut store = GroupSessionStore::new();
        store.add(inbound.clone());

        let loaded_session = store
            .get(&room_id, "test_key", outbound.session_id())
            .unwrap();
        assert_eq!(inbound, loaded_session);
    }

    #[tokio::test]
    async fn test_device_store() {
        let device = get_device();
        let store = DeviceStore::new();

        assert!(store.add(device.clone()));
        assert!(!store.add(device.clone()));

        let loaded_device = store.get(device.user_id(), device.device_id()).unwrap();

        assert_eq!(device, loaded_device);

        let user_devices = store.user_devices(device.user_id());

        assert_eq!(user_devices.keys().next().unwrap(), device.device_id());
        assert_eq!(user_devices.devices().next().unwrap(), &device);

        let loaded_device = user_devices.get(device.device_id()).unwrap();

        assert_eq!(device, loaded_device);

        store.remove(device.user_id(), device.device_id());

        let loaded_device = store.get(device.user_id(), device.device_id());
        assert!(loaded_device.is_none());
    }
}
