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

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{Account, CryptoStore, InboundGroupSession, Result, Session};
use crate::crypto::device::Device;
use crate::crypto::memory_stores::{DeviceStore, GroupSessionStore, SessionStore, UserDevices};
use crate::identifiers::{DeviceId, RoomId, UserId};

#[derive(Debug)]
pub struct MemoryStore {
    sessions: SessionStore,
    inbound_group_sessions: GroupSessionStore,
    tracked_users: HashSet<UserId>,
    devices: DeviceStore,
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore {
            sessions: SessionStore::new(),
            inbound_group_sessions: GroupSessionStore::new(),
            tracked_users: HashSet::new(),
            devices: DeviceStore::new(),
        }
    }
}

#[async_trait]
impl CryptoStore for MemoryStore {
    async fn load_account(&mut self) -> Result<Option<Account>> {
        Ok(None)
    }

    async fn save_account(&mut self, _: Account) -> Result<()> {
        Ok(())
    }

    async fn save_session(&mut self, session: Session) -> Result<()> {
        self.sessions.add(session).await;
        Ok(())
    }

    async fn get_sessions(&mut self, sender_key: &str) -> Result<Option<Arc<Mutex<Vec<Session>>>>> {
        Ok(self.sessions.get(sender_key))
    }

    async fn save_inbound_group_session(&mut self, session: InboundGroupSession) -> Result<bool> {
        Ok(self.inbound_group_sessions.add(session))
    }

    async fn get_inbound_group_session(
        &mut self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<Option<InboundGroupSession>> {
        Ok(self
            .inbound_group_sessions
            .get(room_id, sender_key, session_id))
    }

    fn tracked_users(&self) -> &HashSet<UserId> {
        &self.tracked_users
    }

    async fn add_user_for_tracking(&mut self, user: &UserId) -> Result<bool> {
        Ok(self.tracked_users.insert(user.clone()))
    }

    async fn get_device(&self, user_id: &UserId, device_id: &DeviceId) -> Result<Option<Device>> {
        Ok(self.devices.get(user_id, device_id))
    }

    async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices> {
        Ok(self.devices.user_devices(user_id))
    }

    async fn save_device(&self, device: Device) -> Result<()> {
        self.devices.add(device);
        Ok(())
    }
}
