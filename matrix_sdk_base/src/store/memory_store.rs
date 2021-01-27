// Copyright 2021 The Matrix.org Foundation C.I.C.
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
    collections::BTreeSet,
    sync::{Arc, RwLock},
};

use dashmap::{DashMap, DashSet};
use matrix_sdk_common::{
    async_trait,
    events::{
        presence::PresenceEvent,
        room::member::{MemberEventContent, MembershipState},
        AnyBasicEvent, AnyStrippedStateEvent, AnySyncStateEvent, EventContent, EventType,
    },
    identifiers::{RoomId, UserId},
    instant::Instant,
};

use tracing::info;

use crate::deserialized_responses::{MemberEvent, StrippedMemberEvent};

use super::{Result, RoomInfo, StateChanges, StateStore, StrippedRoomInfo};

#[derive(Debug, Clone)]
pub struct MemoryStore {
    sync_token: Arc<RwLock<Option<String>>>,
    filters: Arc<DashMap<String, String>>,
    account_data: Arc<DashMap<String, AnyBasicEvent>>,
    members: Arc<DashMap<RoomId, DashMap<UserId, MemberEvent>>>,
    profiles: Arc<DashMap<RoomId, DashMap<UserId, MemberEventContent>>>,
    display_names: Arc<DashMap<RoomId, DashMap<String, BTreeSet<UserId>>>>,
    joined_user_ids: Arc<DashMap<RoomId, DashSet<UserId>>>,
    invited_user_ids: Arc<DashMap<RoomId, DashSet<UserId>>>,
    room_info: Arc<DashMap<RoomId, RoomInfo>>,
    #[allow(clippy::type_complexity)]
    room_state: Arc<DashMap<RoomId, DashMap<String, DashMap<String, AnySyncStateEvent>>>>,
    room_account_data: Arc<DashMap<RoomId, DashMap<String, AnyBasicEvent>>>,
    stripped_room_info: Arc<DashMap<RoomId, StrippedRoomInfo>>,
    #[allow(clippy::type_complexity)]
    stripped_room_state:
        Arc<DashMap<RoomId, DashMap<String, DashMap<String, AnyStrippedStateEvent>>>>,
    stripped_members: Arc<DashMap<RoomId, DashMap<UserId, StrippedMemberEvent>>>,
    presence: Arc<DashMap<UserId, PresenceEvent>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            sync_token: Arc::new(RwLock::new(None)),
            filters: DashMap::new().into(),
            account_data: DashMap::new().into(),
            members: DashMap::new().into(),
            profiles: DashMap::new().into(),
            display_names: DashMap::new().into(),
            joined_user_ids: DashMap::new().into(),
            invited_user_ids: DashMap::new().into(),
            room_info: DashMap::new().into(),
            room_state: DashMap::new().into(),
            room_account_data: DashMap::new().into(),
            stripped_room_info: DashMap::new().into(),
            stripped_room_state: DashMap::new().into(),
            stripped_members: DashMap::new().into(),
            presence: DashMap::new().into(),
        }
    }

    pub async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.filters
            .insert(filter_name.to_string(), filter_id.to_string());

        Ok(())
    }

    pub async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        Ok(self.filters.get(filter_name).map(|f| f.to_string()))
    }

    pub async fn get_sync_token(&self) -> Result<Option<String>> {
        Ok(self.sync_token.read().unwrap().clone())
    }

    pub async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let now = Instant::now();

        if let Some(s) = &changes.sync_token {
            *self.sync_token.write().unwrap() = Some(s.to_owned());
        }

        for (room, events) in &changes.members {
            for event in events.values() {
                match event.content.membership {
                    MembershipState::Join => {
                        self.joined_user_ids
                            .entry(room.clone())
                            .or_insert_with(DashSet::new)
                            .insert(event.state_key.clone());
                        self.invited_user_ids
                            .entry(room.clone())
                            .or_insert_with(DashSet::new)
                            .remove(&event.state_key);
                    }
                    MembershipState::Invite => {
                        self.invited_user_ids
                            .entry(room.clone())
                            .or_insert_with(DashSet::new)
                            .insert(event.state_key.clone());
                        self.joined_user_ids
                            .entry(room.clone())
                            .or_insert_with(DashSet::new)
                            .remove(&event.state_key);
                    }
                    _ => {
                        self.joined_user_ids
                            .entry(room.clone())
                            .or_insert_with(DashSet::new)
                            .remove(&event.state_key);
                        self.invited_user_ids
                            .entry(room.clone())
                            .or_insert_with(DashSet::new)
                            .remove(&event.state_key);
                    }
                }

                self.members
                    .entry(room.clone())
                    .or_insert_with(DashMap::new)
                    .insert(event.state_key.clone(), event.clone());
            }
        }

        for (room, users) in &changes.profiles {
            for (user_id, profile) in users {
                self.profiles
                    .entry(room.clone())
                    .or_insert_with(DashMap::new)
                    .insert(user_id.clone(), profile.clone());
            }
        }

        for (room, map) in &changes.ambiguity_maps {
            for (display_name, display_names) in map {
                self.display_names
                    .entry(room.clone())
                    .or_insert_with(DashMap::new)
                    .insert(display_name.clone(), display_names.clone());
            }
        }

        for (event_type, event) in &changes.account_data {
            self.account_data
                .insert(event_type.to_string(), event.clone());
        }

        for (room, events) in &changes.room_account_data {
            for (event_type, event) in events {
                self.room_account_data
                    .entry(room.clone())
                    .or_insert_with(DashMap::new)
                    .insert(event_type.to_string(), event.clone());
            }
        }

        for (room, event_types) in &changes.state {
            for events in event_types.values() {
                for event in events.values() {
                    self.room_state
                        .entry(room.clone())
                        .or_insert_with(DashMap::new)
                        .entry(event.content().event_type().to_string())
                        .or_insert_with(DashMap::new)
                        .insert(event.state_key().to_string(), event.clone());
                }
            }
        }

        for (room_id, room_info) in &changes.room_infos {
            self.room_info.insert(room_id.clone(), room_info.clone());
        }

        for (sender, event) in &changes.presence {
            self.presence.insert(sender.clone(), event.clone());
        }

        for (room_id, info) in &changes.invited_room_info {
            self.stripped_room_info
                .insert(room_id.clone(), info.clone());
        }

        for (room, events) in &changes.stripped_members {
            for event in events.values() {
                self.stripped_members
                    .entry(room.clone())
                    .or_insert_with(DashMap::new)
                    .insert(event.state_key.clone(), event.clone());
            }
        }

        for (room, event_types) in &changes.stripped_state {
            for events in event_types.values() {
                for event in events.values() {
                    self.stripped_room_state
                        .entry(room.clone())
                        .or_insert_with(DashMap::new)
                        .entry(event.content().event_type().to_string())
                        .or_insert_with(DashMap::new)
                        .insert(event.state_key().to_string(), event.clone());
                }
            }
        }

        info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    pub async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<PresenceEvent>> {
        #[allow(clippy::map_clone)]
        Ok(self.presence.get(user_id).map(|p| p.clone()))
    }

    pub async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<AnySyncStateEvent>> {
        #[allow(clippy::map_clone)]
        Ok(self.room_state.get(room_id).and_then(|e| {
            e.get(event_type.as_ref())
                .and_then(|s| s.get(state_key).map(|e| e.clone()))
        }))
    }

    pub async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>> {
        #[allow(clippy::map_clone)]
        Ok(self
            .profiles
            .get(room_id)
            .and_then(|p| p.get(user_id).map(|p| p.clone())))
    }

    pub async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        #[allow(clippy::map_clone)]
        Ok(self
            .members
            .get(room_id)
            .and_then(|m| m.get(state_key).map(|m| m.clone())))
    }

    fn get_invited_user_ids(&self, room_id: &RoomId) -> Vec<UserId> {
        #[allow(clippy::map_clone)]
        self.invited_user_ids
            .get(room_id)
            .map(|u| u.iter().map(|u| u.clone()).collect())
            .unwrap_or_default()
    }

    fn get_joined_user_ids(&self, room_id: &RoomId) -> Vec<UserId> {
        #[allow(clippy::map_clone)]
        self.joined_user_ids
            .get(room_id)
            .map(|u| u.iter().map(|u| u.clone()).collect())
            .unwrap_or_default()
    }

    fn get_room_infos(&self) -> Vec<RoomInfo> {
        #[allow(clippy::map_clone)]
        self.room_info.iter().map(|r| r.clone()).collect()
    }

    fn get_stripped_room_infos(&self) -> Vec<StrippedRoomInfo> {
        #[allow(clippy::map_clone)]
        self.stripped_room_info.iter().map(|r| r.clone()).collect()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl StateStore for MemoryStore {
    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.save_filter(filter_name, filter_id).await
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        self.save_changes(changes).await
    }

    async fn get_filter(&self, filter_id: &str) -> Result<Option<String>> {
        self.get_filter(filter_id).await
    }

    async fn get_sync_token(&self) -> Result<Option<String>> {
        self.get_sync_token().await
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<PresenceEvent>> {
        self.get_presence_event(user_id).await
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<AnySyncStateEvent>> {
        self.get_state_event(room_id, event_type, state_key).await
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>> {
        self.get_profile(room_id, user_id).await
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        self.get_member_event(room_id, state_key).await
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        Ok(self.get_invited_user_ids(room_id))
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        Ok(self.get_joined_user_ids(room_id))
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        Ok(self.get_room_infos())
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<StrippedRoomInfo>> {
        Ok(self.get_stripped_room_infos())
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<UserId>> {
        #[allow(clippy::map_clone)]
        Ok(self
            .display_names
            .get(room_id)
            .and_then(|d| d.get(display_name).map(|d| d.clone()))
            .unwrap_or_default())
    }
}
