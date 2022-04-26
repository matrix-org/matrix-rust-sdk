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
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{Arc, RwLock},
};

use async_stream::stream;
use dashmap::{DashMap, DashSet};
use lru::LruCache;
#[allow(unused_imports)]
use matrix_sdk_common::{async_trait, instant::Instant, locks::Mutex};
use ruma::{
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::{
            member::{
                MembershipState, OriginalSyncRoomMemberEvent, RoomMemberEventContent,
                StrippedRoomMemberEvent,
            },
            redaction::SyncRoomRedactionEvent,
        },
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncMessageLikeEvent, AnySyncRoomEvent, AnySyncStateEvent, GlobalAccountDataEventType,
        RoomAccountDataEventType, StateEventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    signatures::{redact_in_place, CanonicalJsonObject},
    EventId, MxcUri, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, RoomVersionId, UserId,
};
#[allow(unused_imports)]
use tracing::{info, warn};

use super::{BoxStream, Result, RoomInfo, StateChanges, StateStore};
use crate::{
    deserialized_responses::{MemberEvent, SyncRoomEvent},
    media::{MediaRequest, UniqueKey},
    StoreError,
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
    members: Arc<DashMap<OwnedRoomId, DashMap<OwnedUserId, OriginalSyncRoomMemberEvent>>>,
    profiles: Arc<DashMap<OwnedRoomId, DashMap<OwnedUserId, RoomMemberEventContent>>>,
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
    stripped_members: Arc<DashMap<OwnedRoomId, DashMap<OwnedUserId, StrippedRoomMemberEvent>>>,
    presence: Arc<DashMap<OwnedUserId, Raw<PresenceEvent>>>,
    room_user_receipts:
        Arc<DashMap<OwnedRoomId, DashMap<String, DashMap<OwnedUserId, (OwnedEventId, Receipt)>>>>,
    room_event_receipts: Arc<
        DashMap<OwnedRoomId, DashMap<String, DashMap<OwnedEventId, DashMap<OwnedUserId, Receipt>>>>,
    >,
    media: Arc<Mutex<LruCache<String, Vec<u8>>>>,
    custom: Arc<DashMap<Vec<u8>, Vec<u8>>>,
    room_timeline: Arc<DashMap<OwnedRoomId, TimelineData>>,
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
            presence: Default::default(),
            room_user_receipts: Default::default(),
            room_event_receipts: Default::default(),
            media: Arc::new(Mutex::new(LruCache::new(100))),
            custom: DashMap::new().into(),
            room_timeline: Default::default(),
        }
    }

    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.filters.insert(filter_name.to_owned(), filter_id.to_owned());

        Ok(())
    }

    async fn get_filter(&self, filter_name: &str) -> Result<Option<String>> {
        Ok(self.filters.get(filter_name).map(|f| f.to_string()))
    }

    async fn get_sync_token(&self) -> Result<Option<String>> {
        Ok(self.sync_token.read().unwrap().clone())
    }

    async fn save_changes(&self, changes: &StateChanges) -> Result<()> {
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
            self.account_data.insert(event_type.clone(), event.clone());
        }

        for (room, events) in &changes.room_account_data {
            for (event_type, event) in events {
                self.room_account_data
                    .entry(room.clone())
                    .or_insert_with(DashMap::new)
                    .insert(event_type.clone(), event.clone());
            }
        }

        for (room, event_types) in &changes.state {
            for (event_type, events) in event_types {
                for (state_key, event) in events {
                    self.room_state
                        .entry(room.clone())
                        .or_insert_with(DashMap::new)
                        .entry(event_type.clone())
                        .or_insert_with(DashMap::new)
                        .insert(state_key.to_owned(), event.clone());
                }
            }
        }

        for (room_id, room_info) in &changes.room_infos {
            self.room_info.insert(room_id.clone(), room_info.clone());
        }

        for (sender, event) in &changes.presence {
            self.presence.insert(sender.clone(), event.clone());
        }

        for (room_id, info) in &changes.stripped_room_infos {
            self.stripped_room_infos.insert(room_id.clone(), info.clone());
        }

        for (room, events) in &changes.stripped_members {
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

                self.stripped_members
                    .entry(room.clone())
                    .or_insert_with(DashMap::new)
                    .insert(event.state_key.clone(), event.clone());
            }
        }

        for (room, event_types) in &changes.stripped_state {
            for (event_type, events) in event_types {
                for (state_key, event) in events {
                    self.stripped_room_state
                        .entry(room.clone())
                        .or_insert_with(DashMap::new)
                        .entry(event_type.clone())
                        .or_insert_with(DashMap::new)
                        .insert(state_key.to_owned(), event.clone());
                }
            }
        }

        for (room, content) in &changes.receipts {
            for (event_id, receipts) in &content.0 {
                for (receipt_type, receipts) in receipts {
                    for (user_id, receipt) in receipts {
                        // Add the receipt to the room user receipts
                        if let Some((old_event, _)) = self
                            .room_user_receipts
                            .entry(room.clone())
                            .or_insert_with(DashMap::new)
                            .entry(receipt_type.to_string())
                            .or_insert_with(DashMap::new)
                            .insert(user_id.clone(), (event_id.clone(), receipt.clone()))
                        {
                            // Remove the old receipt from the room event receipts
                            if let Some(receipt_map) = self.room_event_receipts.get(room) {
                                if let Some(event_map) = receipt_map.get(receipt_type.as_ref()) {
                                    if let Some(user_map) = event_map.get_mut(&old_event) {
                                        user_map.remove(user_id);
                                    }
                                }
                            }
                        }

                        // Add the receipt to the room event receipts
                        self.room_event_receipts
                            .entry(room.clone())
                            .or_insert_with(DashMap::new)
                            .entry(receipt_type.to_string())
                            .or_insert_with(DashMap::new)
                            .entry(event_id.clone())
                            .or_insert_with(DashMap::new)
                            .insert(user_id.clone(), receipt.clone());
                    }
                }
            }
        }

        for (room, timeline) in &changes.timeline {
            if timeline.sync {
                info!("Save new timeline batch from sync response for {}", room);
            } else {
                info!("Save new timeline batch from messages response for {}", room);
            }

            let mut delete_timeline = false;
            if timeline.limited {
                info!("Delete stored timeline for {} because the sync response was limited", room);
                delete_timeline = true;
            } else if let Some(mut data) = self.room_timeline.get_mut(room) {
                if !timeline.sync && Some(&timeline.start) != data.end.as_ref() {
                    // This should only happen when a developer adds a wrong timeline
                    // batch to the `StateChanges` or the server returns a wrong response
                    // to our request.
                    warn!("Drop unexpected timeline batch for {}", room);
                    return Ok(());
                }

                // Check if the event already exists in the store
                for event in &timeline.events {
                    if let Some(event_id) = event.event_id() {
                        if data.event_id_to_position.contains_key(&event_id) {
                            delete_timeline = true;
                            break;
                        }
                    }
                }

                if !delete_timeline {
                    if timeline.sync {
                        data.start = timeline.start.clone();
                    } else {
                        data.end = timeline.end.clone();
                    }
                }
            }

            if delete_timeline {
                info!("Delete stored timeline for {} because of duplicated events", room);
                self.room_timeline.remove(room);
            }

            let mut data =
                self.room_timeline.entry(room.to_owned()).or_insert_with(|| TimelineData {
                    start: timeline.start.clone(),
                    end: timeline.end.clone(),
                    ..Default::default()
                });

            let make_room_version = || {
                self.room_info
                    .get(room)
                    .and_then(|info| {
                        info.base_info.create.as_ref().map(|event| event.room_version.clone())
                    })
                    .unwrap_or_else(|| {
                        warn!("Unable to find the room version for {}, assume version 9", room);
                        RoomVersionId::V9
                    })
            };

            if timeline.sync {
                let mut room_version = None;
                for event in &timeline.events {
                    // Redact events already in store only on sync response
                    if let Ok(AnySyncRoomEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomRedaction(SyncRoomRedactionEvent::Original(
                            redaction,
                        )),
                    )) = event.event.deserialize()
                    {
                        let pos = data.event_id_to_position.get(&redaction.redacts).copied();

                        if let Some(position) = pos {
                            if let Some(mut full_event) = data.events.get_mut(&position.clone()) {
                                let mut event_json: CanonicalJsonObject =
                                    full_event.event.deserialize_as()?;
                                let v = room_version.get_or_insert_with(make_room_version);

                                redact_in_place(&mut event_json, v)
                                    .map_err(StoreError::Redaction)?;
                                full_event.event = Raw::new(&event_json)?.cast();
                            }
                        }
                    }

                    data.start_position -= 1;
                    let start_position = data.start_position;
                    // Only add event with id to the position map
                    if let Some(event_id) = event.event_id() {
                        data.event_id_to_position.insert(event_id, start_position);
                    }
                    data.events.insert(start_position, event.clone());
                }
            } else {
                for event in &timeline.events {
                    data.end_position += 1;
                    let end_position = data.end_position;
                    // Only add event with id to the position map
                    if let Some(event_id) = event.event_id() {
                        data.event_id_to_position.insert(event_id, end_position);
                    }
                    data.events.insert(end_position, event.clone());
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
    ) -> Result<Option<RoomMemberEventContent>> {
        Ok(self.profiles.get(room_id).and_then(|p| p.get(user_id).map(|p| p.clone())))
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        if let Some(e) = self.members.get(room_id).and_then(|m| m.get(state_key).map(|m| m.clone()))
        {
            Ok(Some(e.into()))
        } else if let Some(e) =
            self.stripped_members.get(room_id).and_then(|m| m.get(state_key).map(|m| m.clone()))
        {
            Ok(Some(e.into()))
        } else {
            Ok(None)
        }
    }

    fn get_user_ids(&self, room_id: &RoomId) -> Vec<OwnedUserId> {
        if let Some(u) = self.members.get(room_id) {
            u.iter().map(|u| u.key().clone()).collect()
        } else {
            self.stripped_members
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
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        Ok(self.room_user_receipts.get(room_id).and_then(|m| {
            m.get(receipt_type.as_ref()).and_then(|m| m.get(user_id).map(|r| r.clone()))
        }))
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        Ok(self
            .room_event_receipts
            .get(room_id)
            .and_then(|m| {
                m.get(receipt_type.as_ref()).and_then(|m| {
                    m.get(event_id)
                        .map(|m| m.iter().map(|r| (r.key().clone(), r.value().clone())).collect())
                })
            })
            .unwrap_or_default())
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.custom.get(key).map(|e| e.value().clone()))
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        Ok(self.custom.insert(key.to_vec(), value))
    }

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        self.media.lock().await.put(request.unique_key(), data);

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        Ok(self.media.lock().await.get(&request.unique_key()).cloned())
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        self.media.lock().await.pop(&request.unique_key());

        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let mut media_store = self.media.lock().await;

        let keys: Vec<String> = media_store
            .iter()
            .filter_map(
                |(key, _)| if key.starts_with(&uri.to_string()) { Some(key.clone()) } else { None },
            )
            .collect();

        for key in keys {
            media_store.pop(&key);
        }

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
        self.room_timeline.remove(room_id);

        Ok(())
    }

    async fn room_timeline(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<(BoxStream<Result<SyncRoomEvent>>, Option<String>)>> {
        let (events, end_token) = if let Some(data) = self.room_timeline.get(room_id) {
            (data.events.clone(), data.end.clone())
        } else {
            info!("No timeline for {} was previously stored", room_id);
            return Ok(None);
        };

        let stream = stream! {
            for (_, item) in events {
                yield Ok(item);
            }
        };

        info!("Found previously stored timeline for {}, with end token {:?}", room_id, end_token);

        Ok(Some((Box::pin(stream), end_token)))
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
    ) -> Result<Option<RoomMemberEventContent>> {
        self.get_profile(room_id, user_id).await
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        self.get_member_event(room_id, state_key).await
    }

    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        Ok(self.get_user_ids(room_id))
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        Ok(self.get_invited_user_ids(room_id))
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
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
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, user_id).await
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        self.get_event_room_receipt_events(room_id, receipt_type, event_id).await
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

    async fn room_timeline(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<(BoxStream<Result<SyncRoomEvent>>, Option<String>)>> {
        self.room_timeline(room_id).await
    }
}

#[derive(Debug, Default)]
struct TimelineData {
    pub start: String,
    pub start_position: isize,
    pub end: Option<String>,
    pub end_position: isize,
    pub events: BTreeMap<isize, SyncRoomEvent>,
    pub event_id_to_position: HashMap<OwnedEventId, isize>,
}

#[cfg(test)]
mod tests {

    use super::{MemoryStore, Result, StateStore};

    async fn get_store() -> Result<impl StateStore> {
        Ok(MemoryStore::new())
    }

    statestore_integration_tests! { integration }
}
