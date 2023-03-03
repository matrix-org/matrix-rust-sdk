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

use async_trait::async_trait;
use dashmap::{DashMap, DashSet};
#[allow(unused_imports)]
use matrix_sdk_common::{instant::Instant, locks::Mutex};
use ruma::{
    canonical_json::redact,
    events::{
        presence::PresenceEvent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::member::{MembershipState, StrippedRoomMemberEvent, SyncRoomMemberEvent},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    serde::Raw,
    CanonicalJsonObject, EventId, MxcUri, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId,
    RoomVersionId, UserId,
};
use tracing::{debug, info, warn};

use super::{Result, RoomInfo, StateChanges, StateStore, StoreError};
use crate::{
    deserialized_responses::RawMemberEvent, media::MediaRequest, MinimalRoomMemberEvent,
    StateStoreDataKey, StateStoreDataValue,
};

/// In-Memory, non-persistent implementation of the `StateStore`
///
/// Default if no other is configured at startup.
#[allow(clippy::type_complexity)]
#[derive(Debug, Clone)]
pub struct MemoryStore {
    sync_token: Arc<RwLock<Option<String>>>,
    filters: Arc<DashMap<String, String>>,
    account_data: Arc<DashMap<GlobalAccountDataEventType, Raw<AnyGlobalAccountDataEvent>>>,
    members: Arc<DashMap<OwnedRoomId, DashMap<OwnedUserId, Raw<SyncRoomMemberEvent>>>>,
    profiles: Arc<DashMap<OwnedRoomId, DashMap<OwnedUserId, MinimalRoomMemberEvent>>>,
    display_names: Arc<DashMap<OwnedRoomId, DashMap<String, BTreeSet<OwnedUserId>>>>,
    joined_user_ids: Arc<DashMap<OwnedRoomId, DashSet<OwnedUserId>>>,
    invited_user_ids: Arc<DashMap<OwnedRoomId, DashSet<OwnedUserId>>>,
    room_info: Arc<DashMap<OwnedRoomId, RoomInfo>>,
    room_state:
        Arc<DashMap<OwnedRoomId, DashMap<StateEventType, DashMap<String, Raw<AnySyncStateEvent>>>>>,
    room_account_data:
        Arc<DashMap<OwnedRoomId, DashMap<RoomAccountDataEventType, Raw<AnyRoomAccountDataEvent>>>>,
    stripped_room_infos: Arc<DashMap<OwnedRoomId, RoomInfo>>,
    stripped_room_state: Arc<
        DashMap<OwnedRoomId, DashMap<StateEventType, DashMap<String, Raw<AnyStrippedStateEvent>>>>,
    >,
    stripped_members: Arc<DashMap<OwnedRoomId, DashMap<OwnedUserId, Raw<StrippedRoomMemberEvent>>>>,
    stripped_joined_user_ids: Arc<DashMap<OwnedRoomId, DashSet<OwnedUserId>>>,
    stripped_invited_user_ids: Arc<DashMap<OwnedRoomId, DashSet<OwnedUserId>>>,
    presence: Arc<DashMap<OwnedUserId, Raw<PresenceEvent>>>,
    room_user_receipts: Arc<
        DashMap<
            OwnedRoomId,
            DashMap<(String, Option<String>), DashMap<OwnedUserId, (OwnedEventId, Receipt)>>,
        >,
    >,
    room_event_receipts: Arc<
        DashMap<
            OwnedRoomId,
            DashMap<(String, Option<String>), DashMap<OwnedEventId, DashMap<OwnedUserId, Receipt>>>,
        >,
    >,
    custom: Arc<DashMap<Vec<u8>, Vec<u8>>>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    #[allow(dead_code)]
    /// Create a new empty MemoryStore
    pub fn new() -> Self {
        Self {
            sync_token: Default::default(),
            filters: Default::default(),
            account_data: Default::default(),
            members: Default::default(),
            profiles: Default::default(),
            display_names: Default::default(),
            joined_user_ids: Default::default(),
            invited_user_ids: Default::default(),
            room_info: Default::default(),
            room_state: Default::default(),
            room_account_data: Default::default(),
            stripped_room_infos: Default::default(),
            stripped_room_state: Default::default(),
            stripped_members: Default::default(),
            stripped_joined_user_ids: Default::default(),
            stripped_invited_user_ids: Default::default(),
            presence: Default::default(),
            room_user_receipts: Default::default(),
            room_event_receipts: Default::default(),
            #[cfg(feature = "memory-media-cache")]
            media: Arc::new(Mutex::new(LruCache::new(
                100.try_into().expect("100 is a non-zero usize"),
            ))),
            custom: DashMap::new().into(),
        }
    }

    async fn get_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<Option<StateStoreDataValue>> {
        match key {
            StateStoreDataKey::SyncToken => {
                Ok(self.sync_token.read().unwrap().clone().map(StateStoreDataValue::SyncToken))
            }
            StateStoreDataKey::Filter(filter_name) => Ok(self
                .filters
                .get(filter_name)
                .map(|f| StateStoreDataValue::Filter(f.value().clone()))),
        }
    }

    async fn set_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
        value: StateStoreDataValue,
    ) -> Result<()> {
        match key {
            StateStoreDataKey::SyncToken => {
                *self.sync_token.write().unwrap() =
                    Some(value.into_sync_token().expect("Session data not a sync token"))
            }
            StateStoreDataKey::Filter(filter_name) => {
                self.filters.insert(
                    filter_name.to_owned(),
                    value.into_filter().expect("Session data not a filter"),
                );
            }
        }

        Ok(())
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<()> {
        match key {
            StateStoreDataKey::SyncToken => *self.sync_token.write().unwrap() = None,
            StateStoreDataKey::Filter(filter_name) => {
                self.filters.remove(filter_name);
            }
        }

        Ok(())
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let now = Instant::now();

        if let Some(s) = &changes.sync_token {
            *self.sync_token.write().unwrap() = Some(s.to_owned());
        }

        for (room, raw_events) in &changes.members {
            for raw_event in raw_events.values() {
                let event = match raw_event.deserialize() {
                    Ok(ev) => ev,
                    Err(e) => {
                        let event_id: Option<String> =
                            raw_event.get_field("event_id").ok().flatten();
                        debug!(event_id, "Failed to deserialize member event: {e}");
                        continue;
                    }
                };

                self.stripped_joined_user_ids.remove(room);
                self.stripped_invited_user_ids.remove(room);

                match event.membership() {
                    MembershipState::Join => {
                        self.joined_user_ids
                            .entry(room.clone())
                            .or_default()
                            .insert(event.state_key().to_owned());
                        self.invited_user_ids
                            .entry(room.clone())
                            .or_default()
                            .remove(event.state_key());
                    }
                    MembershipState::Invite => {
                        self.invited_user_ids
                            .entry(room.clone())
                            .or_default()
                            .insert(event.state_key().to_owned());
                        self.joined_user_ids
                            .entry(room.clone())
                            .or_default()
                            .remove(event.state_key());
                    }
                    _ => {
                        self.joined_user_ids
                            .entry(room.clone())
                            .or_default()
                            .remove(event.state_key());
                        self.invited_user_ids
                            .entry(room.clone())
                            .or_default()
                            .remove(event.state_key());
                    }
                }

                self.members
                    .entry(room.clone())
                    .or_default()
                    .insert(event.state_key().to_owned(), raw_event.clone());
                self.stripped_members.remove(room);
            }
        }

        for (room, users) in &changes.profiles {
            for (user_id, profile) in users {
                self.profiles
                    .entry(room.clone())
                    .or_default()
                    .insert(user_id.clone(), profile.clone());
            }
        }

        for (room, map) in &changes.ambiguity_maps {
            for (display_name, display_names) in map {
                self.display_names
                    .entry(room.clone())
                    .or_default()
                    .insert(display_name.clone(), display_names.clone());
            }
        }

        for (event_type, event) in &changes.account_data {
            self.account_data.insert(event_type.clone(), event.clone());
        }

        for (room, events) in &changes.room_account_data {
            for (event_type, event) in events {
                self.room_account_data
                    .entry(room.clone())
                    .or_default()
                    .insert(event_type.clone(), event.clone());
            }
        }

        for (room, event_types) in &changes.state {
            for (event_type, events) in event_types {
                for (state_key, event) in events {
                    self.room_state
                        .entry(room.clone())
                        .or_default()
                        .entry(event_type.clone())
                        .or_default()
                        .insert(state_key.to_owned(), event.clone());
                    self.stripped_room_state.remove(room);
                }
            }
        }

        for (room_id, room_info) in &changes.room_infos {
            self.room_info.insert(room_id.clone(), room_info.clone());
            self.stripped_room_infos.remove(room_id);
        }

        for (sender, event) in &changes.presence {
            self.presence.insert(sender.clone(), event.clone());
        }

        for (room_id, info) in &changes.stripped_room_infos {
            self.stripped_room_infos.insert(room_id.clone(), info.clone());
            self.room_info.remove(room_id);
        }

        for (room, raw_events) in &changes.stripped_members {
            for raw_event in raw_events.values() {
                let event = match raw_event.deserialize() {
                    Ok(ev) => ev,
                    Err(e) => {
                        let event_id: Option<String> =
                            raw_event.get_field("event_id").ok().flatten();
                        debug!(event_id, "Failed to deserialize stripped member event: {e}");
                        continue;
                    }
                };

                match event.content.membership {
                    MembershipState::Join => {
                        self.stripped_joined_user_ids
                            .entry(room.clone())
                            .or_default()
                            .insert(event.state_key.clone());
                        self.stripped_invited_user_ids
                            .entry(room.clone())
                            .or_default()
                            .remove(&event.state_key);
                    }
                    MembershipState::Invite => {
                        self.stripped_invited_user_ids
                            .entry(room.clone())
                            .or_default()
                            .insert(event.state_key.clone());
                        self.stripped_joined_user_ids
                            .entry(room.clone())
                            .or_default()
                            .remove(&event.state_key);
                    }
                    _ => {
                        self.stripped_joined_user_ids
                            .entry(room.clone())
                            .or_default()
                            .remove(&event.state_key);
                        self.stripped_invited_user_ids
                            .entry(room.clone())
                            .or_default()
                            .remove(&event.state_key);
                    }
                }

                self.stripped_members
                    .entry(room.clone())
                    .or_default()
                    .insert(event.state_key.clone(), raw_event.clone());
            }
        }

        for (room, event_types) in &changes.stripped_state {
            for (event_type, events) in event_types {
                for (state_key, event) in events {
                    self.stripped_room_state
                        .entry(room.clone())
                        .or_default()
                        .entry(event_type.clone())
                        .or_default()
                        .insert(state_key.to_owned(), event.clone());
                }
            }
        }

        for (room, content) in &changes.receipts {
            for (event_id, receipts) in &content.0 {
                for (receipt_type, receipts) in receipts {
                    for (user_id, receipt) in receipts {
                        let thread = receipt.thread.as_str().map(ToOwned::to_owned);
                        // Add the receipt to the room user receipts
                        if let Some((old_event, _)) = self
                            .room_user_receipts
                            .entry(room.clone())
                            .or_default()
                            .entry((receipt_type.to_string(), thread.clone()))
                            .or_default()
                            .insert(user_id.clone(), (event_id.clone(), receipt.clone()))
                        {
                            // Remove the old receipt from the room event receipts
                            if let Some(receipt_map) = self.room_event_receipts.get(room) {
                                if let Some(event_map) =
                                    receipt_map.get(&(receipt_type.to_string(), thread.clone()))
                                {
                                    if let Some(user_map) = event_map.get_mut(&old_event) {
                                        user_map.remove(user_id);
                                    }
                                }
                            }
                        }

                        // Add the receipt to the room event receipts
                        self.room_event_receipts
                            .entry(room.clone())
                            .or_default()
                            .entry((receipt_type.to_string(), thread))
                            .or_default()
                            .entry(event_id.clone())
                            .or_default()
                            .insert(user_id.clone(), receipt.clone());
                    }
                }
            }
        }

        let make_room_version = |room_id| {
            self.room_info
                .get(room_id)
                .and_then(|info| info.room_version().cloned())
                .unwrap_or_else(|| {
                    warn!(?room_id, "Unable to find the room version, assuming version 9");
                    RoomVersionId::V9
                })
        };

        for (room_id, redactions) in &changes.redactions {
            let mut room_version = None;
            if let Some(room) = self.room_state.get_mut(room_id) {
                for mut ref_room_mu in room.iter_mut() {
                    for mut ref_evt_mu in ref_room_mu.value_mut().iter_mut() {
                        let raw_evt = ref_evt_mu.value_mut();
                        if let Ok(Some(event_id)) = raw_evt.get_field::<OwnedEventId>("event_id") {
                            if let Some(redaction) = redactions.get(&event_id) {
                                let redacted = redact(
                                    raw_evt.deserialize_as::<CanonicalJsonObject>()?,
                                    room_version.get_or_insert_with(|| make_room_version(room_id)),
                                    Some(redaction.try_into()?),
                                )
                                .map_err(StoreError::Redaction)?;
                                *raw_evt = Raw::new(&redacted)?.cast();
                            }
                        }
                    }
                }
            }
        }

        info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        Ok(self.presence.get(user_id).map(|p| p.clone()))
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        Ok(self
            .room_state
            .get(room_id)
            .and_then(|e| e.get(&event_type).and_then(|s| s.get(state_key).map(|e| e.clone()))))
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        Ok(self
            .room_state
            .get(room_id)
            .and_then(|e| {
                e.get(&event_type).map(|s| s.iter().map(|e| e.clone()).collect::<Vec<_>>())
            })
            .unwrap_or_default())
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>> {
        Ok(self.profiles.get(room_id).and_then(|p| p.get(user_id).map(|p| p.clone())))
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<RawMemberEvent>> {
        if let Some(e) =
            self.stripped_members.get(room_id).and_then(|m| m.get(state_key).map(|m| m.clone()))
        {
            Ok(Some(RawMemberEvent::Stripped(e)))
        } else if let Some(e) =
            self.members.get(room_id).and_then(|m| m.get(state_key).map(|m| m.clone()))
        {
            Ok(Some(RawMemberEvent::Sync(e)))
        } else {
            Ok(None)
        }
    }

    fn get_user_ids(&self, room_id: &RoomId) -> Vec<OwnedUserId> {
        if let Some(u) = self.stripped_members.get(room_id) {
            u.iter().map(|u| u.key().clone()).collect()
        } else {
            self.members
                .get(room_id)
                .map(|u| u.iter().map(|u| u.key().clone()).collect())
                .unwrap_or_default()
        }
    }

    fn get_invited_user_ids(&self, room_id: &RoomId) -> Vec<OwnedUserId> {
        self.invited_user_ids
            .get(room_id)
            .map(|u| u.iter().map(|u| u.clone()).collect())
            .unwrap_or_default()
    }

    fn get_joined_user_ids(&self, room_id: &RoomId) -> Vec<OwnedUserId> {
        self.joined_user_ids
            .get(room_id)
            .map(|u| u.iter().map(|u| u.clone()).collect())
            .unwrap_or_default()
    }

    fn get_stripped_invited_user_ids(&self, room_id: &RoomId) -> Vec<OwnedUserId> {
        self.stripped_invited_user_ids
            .get(room_id)
            .map(|u| u.iter().map(|u| u.clone()).collect())
            .unwrap_or_default()
    }

    fn get_stripped_joined_user_ids(&self, room_id: &RoomId) -> Vec<OwnedUserId> {
        self.stripped_joined_user_ids
            .get(room_id)
            .map(|u| u.iter().map(|u| u.clone()).collect())
            .unwrap_or_default()
    }

    fn get_room_infos(&self) -> Vec<RoomInfo> {
        self.room_info.iter().map(|r| r.clone()).collect()
    }

    fn get_stripped_room_infos(&self) -> Vec<RoomInfo> {
        self.stripped_room_infos.iter().map(|r| r.clone()).collect()
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        Ok(self.account_data.get(&event_type).map(|e| e.clone()))
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        Ok(self.room_account_data.get(room_id).and_then(|m| m.get(&event_type).map(|e| e.clone())))
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        Ok(self.room_user_receipts.get(room_id).and_then(|m| {
            m.get(&(receipt_type.to_string(), thread.as_str().map(ToOwned::to_owned)))
                .and_then(|m| m.get(user_id).map(|r| r.clone()))
        }))
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        Ok(self
            .room_event_receipts
            .get(room_id)
            .and_then(|m| {
                m.get(&(receipt_type.to_string(), thread.as_str().map(ToOwned::to_owned))).and_then(
                    |m| {
                        m.get(event_id).map(|m| {
                            m.iter().map(|r| (r.key().clone(), r.value().clone())).collect()
                        })
                    },
                )
            })
            .unwrap_or_default())
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.custom.get(key).map(|e| e.value().clone()))
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        Ok(self.custom.insert(key.to_vec(), value))
    }

    // The in-memory store doesn't cache media
    async fn add_media_content(&self, _request: &MediaRequest, _data: Vec<u8>) -> Result<()> {
        Ok(())
    }
    async fn get_media_content(&self, _request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    async fn remove_media_content(&self, _request: &MediaRequest) -> Result<()> {
        Ok(())
    }
    async fn remove_media_content_for_uri(&self, _uri: &MxcUri) -> Result<()> {
        Ok(())
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        self.members.remove(room_id);
        self.profiles.remove(room_id);
        self.display_names.remove(room_id);
        self.joined_user_ids.remove(room_id);
        self.invited_user_ids.remove(room_id);
        self.room_info.remove(room_id);
        self.room_state.remove(room_id);
        self.room_account_data.remove(room_id);
        self.stripped_room_infos.remove(room_id);
        self.stripped_room_state.remove(room_id);
        self.stripped_members.remove(room_id);
        self.room_user_receipts.remove(room_id);
        self.room_event_receipts.remove(room_id);

        Ok(())
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl StateStore for MemoryStore {
    type Error = StoreError;

    async fn get_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<Option<StateStoreDataValue>> {
        self.get_kv_data(key).await
    }

    async fn set_kv_data(
        &self,
        key: StateStoreDataKey<'_>,
        value: StateStoreDataValue,
    ) -> Result<()> {
        self.set_kv_data(key, value).await
    }

    async fn remove_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<()> {
        self.remove_kv_data(key).await
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        self.save_changes(changes).await
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        self.get_presence_event(user_id).await
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        self.get_state_event(room_id, event_type, state_key).await
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        self.get_state_events(room_id, event_type).await
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>> {
        self.get_profile(room_id, user_id).await
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<RawMemberEvent>> {
        self.get_member_event(room_id, state_key).await
    }

    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        Ok(self.get_user_ids(room_id))
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let v = self.get_stripped_invited_user_ids(room_id);
        if !v.is_empty() {
            return Ok(v);
        }
        Ok(self.get_invited_user_ids(room_id))
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        let v = self.get_stripped_joined_user_ids(room_id);
        if !v.is_empty() {
            return Ok(v);
        }
        Ok(self.get_joined_user_ids(room_id))
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        Ok(self.get_room_infos())
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        Ok(self.get_stripped_room_infos())
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>> {
        Ok(self
            .display_names
            .get(room_id)
            .and_then(|d| d.get(display_name).map(|d| d.clone()))
            .unwrap_or_default())
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.get_account_data_event(event_type).await
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.get_room_account_data_event(room_id, event_type).await
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, thread, user_id).await
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        self.get_event_room_receipt_events(room_id, receipt_type, thread, event_id).await
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_custom_value(key).await
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        self.set_custom_value(key, value).await
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        self.add_media_content(request, data).await
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        self.get_media_content(request).await
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        self.remove_media_content(request).await
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        self.remove_media_content_for_uri(uri).await
    }

    async fn remove_room(&self, room_id: &RoomId) -> Result<()> {
        self.remove_room(room_id).await
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryStore, Result, StateStore};

    async fn get_store() -> Result<impl StateStore> {
        Ok(MemoryStore::new())
    }

    statestore_integration_tests!();
}
