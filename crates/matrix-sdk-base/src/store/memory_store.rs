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
use dashmap::DashMap;
use matrix_sdk_common::instant::Instant;
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
use tracing::{debug, warn};

use super::{Result, RoomInfo, StateChanges, StateStore, StoreError};
use crate::{
    deserialized_responses::RawAnySyncOrStrippedState, media::MediaRequest, MinimalRoomMemberEvent,
    RoomMemberships, RoomState, StateStoreDataKey, StateStoreDataValue,
};

/// In-Memory, non-persistent implementation of the `StateStore`
///
/// Default if no other is configured at startup.
#[allow(clippy::type_complexity)]
#[derive(Debug, Clone)]
pub struct MemoryStore {
    user_avatar_url: Arc<DashMap<String, String>>,
    sync_token: Arc<RwLock<Option<String>>>,
    filters: Arc<DashMap<String, String>>,
    account_data: Arc<DashMap<GlobalAccountDataEventType, Raw<AnyGlobalAccountDataEvent>>>,
    profiles: Arc<DashMap<OwnedRoomId, DashMap<OwnedUserId, MinimalRoomMemberEvent>>>,
    display_names: Arc<DashMap<OwnedRoomId, DashMap<String, BTreeSet<OwnedUserId>>>>,
    members: Arc<DashMap<OwnedRoomId, DashMap<OwnedUserId, MembershipState>>>,
    room_info: Arc<DashMap<OwnedRoomId, RoomInfo>>,
    room_state:
        Arc<DashMap<OwnedRoomId, DashMap<StateEventType, DashMap<String, Raw<AnySyncStateEvent>>>>>,
    room_account_data:
        Arc<DashMap<OwnedRoomId, DashMap<RoomAccountDataEventType, Raw<AnyRoomAccountDataEvent>>>>,
    stripped_room_state: Arc<
        DashMap<OwnedRoomId, DashMap<StateEventType, DashMap<String, Raw<AnyStrippedStateEvent>>>>,
    >,
    stripped_members: Arc<DashMap<OwnedRoomId, DashMap<OwnedUserId, MembershipState>>>,
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
            user_avatar_url: Default::default(),
            sync_token: Default::default(),
            filters: Default::default(),
            account_data: Default::default(),
            profiles: Default::default(),
            display_names: Default::default(),
            members: Default::default(),
            room_info: Default::default(),
            room_state: Default::default(),
            room_account_data: Default::default(),
            stripped_room_state: Default::default(),
            stripped_members: Default::default(),
            presence: Default::default(),
            room_user_receipts: Default::default(),
            room_event_receipts: Default::default(),
            #[cfg(feature = "memory-media-cache")]
            media: Arc::new(tokio::sync::Mutex::new(LruCache::new(
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
            StateStoreDataKey::UserAvatarUrl(user_id) => Ok(self
                .user_avatar_url
                .get(user_id.as_str())
                .map(|u| StateStoreDataValue::UserAvatarUrl(u.value().clone()))),
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
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                self.filters.insert(
                    user_id.to_string(),
                    value.into_user_avatar_url().expect("Session data not a user avatar url"),
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
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                self.filters.remove(user_id.as_str());
            }
        }

        Ok(())
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
        let now = Instant::now();

        if let Some(s) = &changes.sync_token {
            *self.sync_token.write().unwrap() = Some(s.to_owned());
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
                for (state_key, raw_event) in events {
                    self.room_state
                        .entry(room.clone())
                        .or_default()
                        .entry(event_type.clone())
                        .or_default()
                        .insert(state_key.to_owned(), raw_event.clone());
                    self.stripped_room_state.remove(room);

                    if *event_type == StateEventType::RoomMember {
                        let event = match raw_event.deserialize_as::<SyncRoomMemberEvent>() {
                            Ok(ev) => ev,
                            Err(e) => {
                                let event_id: Option<String> =
                                    raw_event.get_field("event_id").ok().flatten();
                                debug!(event_id, "Failed to deserialize member event: {e}");
                                continue;
                            }
                        };

                        self.stripped_members.remove(room);

                        self.members
                            .entry(room.clone())
                            .or_default()
                            .insert(event.state_key().to_owned(), event.membership().clone());
                    }
                }
            }
        }

        for (room_id, room_info) in &changes.room_infos {
            self.room_info.insert(room_id.clone(), room_info.clone());
        }

        for (sender, event) in &changes.presence {
            self.presence.insert(sender.clone(), event.clone());
        }

        for (room, event_types) in &changes.stripped_state {
            for (event_type, events) in event_types {
                for (state_key, raw_event) in events {
                    self.stripped_room_state
                        .entry(room.clone())
                        .or_default()
                        .entry(event_type.clone())
                        .or_default()
                        .insert(state_key.to_owned(), raw_event.clone());

                    if *event_type == StateEventType::RoomMember {
                        let event = match raw_event.deserialize_as::<StrippedRoomMemberEvent>() {
                            Ok(ev) => ev,
                            Err(e) => {
                                let event_id: Option<String> =
                                    raw_event.get_field("event_id").ok().flatten();
                                debug!(
                                    event_id,
                                    "Failed to deserialize stripped member event: {e}"
                                );
                                continue;
                            }
                        };

                        self.stripped_members
                            .entry(room.clone())
                            .or_default()
                            .insert(event.state_key, event.content.membership.clone());
                    }
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

        debug!("Saved changes in {:?}", now.elapsed());

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
    ) -> Result<Option<RawAnySyncOrStrippedState>> {
        if let Some(e) = self
            .stripped_room_state
            .get(room_id)
            .as_ref()
            .and_then(|events| events.get(&event_type))
            .and_then(|m| m.get(state_key).map(|m| m.clone()))
        {
            Ok(Some(RawAnySyncOrStrippedState::Stripped(e)))
        } else if let Some(e) = self
            .room_state
            .get(room_id)
            .as_ref()
            .and_then(|events| events.get(&event_type))
            .and_then(|m| m.get(state_key).map(|m| m.clone()))
        {
            Ok(Some(RawAnySyncOrStrippedState::Sync(e)))
        } else {
            Ok(None)
        }
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        if let Some(v) = self.stripped_room_state.get(room_id).as_ref().and_then(|events| {
            events.get(&event_type).map(|s| {
                s.iter().map(|e| RawAnySyncOrStrippedState::Stripped(e.clone())).collect::<Vec<_>>()
            })
        }) {
            Ok(v)
        } else if let Some(v) = self.room_state.get(room_id).as_ref().and_then(|events| {
            events.get(&event_type).map(|s| {
                s.iter().map(|e| RawAnySyncOrStrippedState::Sync(e.clone())).collect::<Vec<_>>()
            })
        }) {
            Ok(v)
        } else {
            Ok(Vec::new())
        }
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>> {
        Ok(self.profiles.get(room_id).and_then(|p| p.get(user_id).map(|p| p.clone())))
    }

    /// Get the user IDs for the given room with the given memberships and
    /// stripped state.
    ///
    /// If `memberships` is empty, returns all user IDs in the room with the
    /// given stripped state.
    fn get_user_ids_inner(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
        stripped: bool,
    ) -> Vec<OwnedUserId> {
        let map = if stripped { &self.stripped_members } else { &self.members };

        map.get(room_id)
            .map(|u| {
                u.iter()
                    .filter_map(|u| memberships.matches(u.value()).then(|| u.key().clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn get_room_infos(&self) -> Vec<RoomInfo> {
        self.room_info.iter().map(|r| r.clone()).collect()
    }

    fn get_stripped_room_infos(&self) -> Vec<RoomInfo> {
        self.room_info
            .iter()
            .filter_map(|r| match r.state() {
                RoomState::Invited => Some(r.clone()),
                _ => None,
            })
            .collect()
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

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.custom.remove(key).map(|entry| entry.1))
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
        self.profiles.remove(room_id);
        self.display_names.remove(room_id);
        self.members.remove(room_id);
        self.room_info.remove(room_id);
        self.room_state.remove(room_id);
        self.room_account_data.remove(room_id);
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
    ) -> Result<Option<RawAnySyncOrStrippedState>> {
        self.get_state_event(room_id, event_type, state_key).await
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        self.get_state_events(room_id, event_type).await
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>> {
        self.get_profile(room_id, user_id).await
    }

    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>> {
        let v = self.get_user_ids_inner(room_id, memberships, true);
        if !v.is_empty() {
            return Ok(v);
        }
        Ok(self.get_user_ids_inner(room_id, memberships, false))
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        StateStore::get_user_ids(self, room_id, RoomMemberships::INVITE).await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        StateStore::get_user_ids(self, room_id, RoomMemberships::JOIN).await
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

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.remove_custom_value(key).await
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
