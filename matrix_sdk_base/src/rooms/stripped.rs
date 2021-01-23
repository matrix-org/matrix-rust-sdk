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

use std::sync::{Arc, Mutex as SyncMutex};

use matrix_sdk_common::{
    events::AnyStrippedStateEvent,
    identifiers::{RoomId, UserId},
};
use serde::{Deserialize, Serialize};

use crate::store::StateStore;

use super::BaseRoomInfo;

#[derive(Debug, Clone)]
pub struct StrippedRoom {
    room_id: Arc<RoomId>,
    own_user_id: Arc<UserId>,
    inner: Arc<SyncMutex<StrippedRoomInfo>>,
    store: Arc<Box<dyn StateStore>>,
}

impl StrippedRoom {
    pub fn new(own_user_id: &UserId, store: Arc<Box<dyn StateStore>>, room_id: &RoomId) -> Self {
        let room_id = Arc::new(room_id.clone());

        let info = StrippedRoomInfo {
            room_id,
            base_info: BaseRoomInfo::new(),
        };

        Self::restore(own_user_id, store, info)
    }

    pub fn restore(
        own_user_id: &UserId,
        store: Arc<Box<dyn StateStore>>,
        room_info: StrippedRoomInfo,
    ) -> Self {
        Self {
            own_user_id: Arc::new(own_user_id.clone()),
            room_id: room_info.room_id.clone(),
            store,
            inner: Arc::new(SyncMutex::new(room_info)),
        }
    }

    async fn calculate_name(&self) -> String {
        let inner = self.inner.lock().unwrap();

        if let Some(name) = &inner.base_info.name {
            let name = name.trim();
            name.to_string()
        } else if let Some(alias) = &inner.base_info.canonical_alias {
            let alias = alias.alias().trim();
            alias.to_string()
        } else {
            // TODO do the dance with room members to calculate the name
            self.room_id.to_string()
        }
    }

    pub fn own_user_id(&self) -> &UserId {
        &self.own_user_id
    }

    pub(crate) fn clone_info(&self) -> StrippedRoomInfo {
        (*self.inner.lock().unwrap()).clone()
    }

    pub fn is_encrypted(&self) -> bool {
        self.inner.lock().unwrap().base_info.encryption.is_some()
    }

    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    pub async fn display_name(&self) -> String {
        self.calculate_name().await
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StrippedRoomInfo {
    pub room_id: Arc<RoomId>,
    pub base_info: BaseRoomInfo,
}

impl StrippedRoomInfo {
    pub(crate) fn handle_state_event(&mut self, event: &AnyStrippedStateEvent) -> bool {
        self.base_info.handle_state_event(&event.content())
    }
}
