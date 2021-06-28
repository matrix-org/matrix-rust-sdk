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
use lru::LruCache;
use matrix_sdk_common::{async_trait, instant::Instant, locks::Mutex};
use ruma::{
    api::client::r0::message::get_message_events::Direction,
    events::{
        presence::PresenceEvent,
        receipt::Receipt,
        room::member::{MemberEventContent, MembershipState},
        AnyGlobalAccountDataEvent, AnyRoomAccountDataEvent, AnyStrippedStateEvent,
        AnySyncStateEvent, EventType,
    },
    identifiers::{EventId, MxcUri, RoomId, UserId},
    receipt::ReceiptType,
    serde::Raw,
};
use tracing::info;

use super::{Result, RoomInfo, StateChanges, StateStore, StoredTimelineSlice};
use crate::{
    deserialized_responses::{MemberEvent, StrippedMemberEvent, TimelineSlice},
    media::{MediaRequest, UniqueKey},
};

#[derive(Debug, Clone)]
pub struct MemoryStore {
    sync_token: Arc<RwLock<Option<String>>>,
    filters: Arc<DashMap<String, String>>,
    account_data: Arc<DashMap<String, Raw<AnyGlobalAccountDataEvent>>>,
    members: Arc<DashMap<RoomId, DashMap<UserId, MemberEvent>>>,
    profiles: Arc<DashMap<RoomId, DashMap<UserId, MemberEventContent>>>,
    display_names: Arc<DashMap<RoomId, DashMap<String, BTreeSet<UserId>>>>,
    joined_user_ids: Arc<DashMap<RoomId, DashSet<UserId>>>,
    invited_user_ids: Arc<DashMap<RoomId, DashSet<UserId>>>,
    room_info: Arc<DashMap<RoomId, RoomInfo>>,
    #[allow(clippy::type_complexity)]
    room_state: Arc<DashMap<RoomId, DashMap<String, DashMap<String, Raw<AnySyncStateEvent>>>>>,
    room_account_data: Arc<DashMap<RoomId, DashMap<String, Raw<AnyRoomAccountDataEvent>>>>,
    stripped_room_info: Arc<DashMap<RoomId, RoomInfo>>,
    #[allow(clippy::type_complexity)]
    stripped_room_state:
        Arc<DashMap<RoomId, DashMap<String, DashMap<String, Raw<AnyStrippedStateEvent>>>>>,
    stripped_members: Arc<DashMap<RoomId, DashMap<UserId, StrippedMemberEvent>>>,
    presence: Arc<DashMap<UserId, Raw<PresenceEvent>>>,
    #[allow(clippy::type_complexity)]
    room_user_receipts: Arc<DashMap<RoomId, DashMap<String, DashMap<UserId, (EventId, Receipt)>>>>,
    #[allow(clippy::type_complexity)]
    room_event_receipts:
        Arc<DashMap<RoomId, DashMap<String, DashMap<EventId, DashMap<UserId, Receipt>>>>>,
    media: Arc<Mutex<LruCache<String, Vec<u8>>>>,
    timeline_slices: Arc<DashMap<RoomId, DashMap<String, TimelineSlice>>>,
    event_id_to_timeline_slice: Arc<DashMap<RoomId, DashMap<EventId, String>>>,
}

impl MemoryStore {
    #[cfg(not(feature = "sled_state_store"))]
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
            room_user_receipts: DashMap::new().into(),
            room_event_receipts: DashMap::new().into(),
            media: Arc::new(Mutex::new(LruCache::new(100))),
            timeline_slices: DashMap::new().into(),
            event_id_to_timeline_slice: DashMap::new().into(),
        }
    }

    async fn save_filter(&self, filter_name: &str, filter_id: &str) -> Result<()> {
        self.filters.insert(filter_name.to_string(), filter_id.to_string());

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
            self.account_data.insert(event_type.to_string(), event.clone());
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
            for (event_type, events) in event_types {
                for (state_key, event) in events {
                    self.room_state
                        .entry(room.clone())
                        .or_insert_with(DashMap::new)
                        .entry(event_type.to_owned())
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

        for (room_id, info) in &changes.invited_room_info {
            self.stripped_room_info.insert(room_id.clone(), info.clone());
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
            for (event_type, events) in event_types {
                for (state_key, event) in events {
                    self.stripped_room_state
                        .entry(room.clone())
                        .or_insert_with(DashMap::new)
                        .entry(event_type.to_owned())
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
            // FIXME: make sure that we don't add events already known
            for event in &timeline.events {
                let event_id = event.event_id();

                self.event_id_to_timeline_slice
                    .entry(room.clone())
                    .or_insert_with(DashMap::new)
                    .insert(event_id, timeline.start.clone());
            }

            self.timeline_slices
                .entry(room.clone())
                .or_insert_with(DashMap::new)
                .insert(timeline.start.clone(), timeline.clone());
        }

        info!("Saved changes in {:?}", now.elapsed());

        Ok(())
    }

    async fn get_presence_event(&self, user_id: &UserId) -> Result<Option<Raw<PresenceEvent>>> {
        #[allow(clippy::map_clone)]
        Ok(self.presence.get(user_id).map(|p| p.clone()))
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        #[allow(clippy::map_clone)]
        Ok(self.room_state.get(room_id).and_then(|e| {
            e.get(event_type.as_ref()).and_then(|s| s.get(state_key).map(|e| e.clone()))
        }))
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MemberEventContent>> {
        #[allow(clippy::map_clone)]
        Ok(self.profiles.get(room_id).and_then(|p| p.get(user_id).map(|p| p.clone())))
    }

    async fn get_member_event(
        &self,
        room_id: &RoomId,
        state_key: &UserId,
    ) -> Result<Option<MemberEvent>> {
        #[allow(clippy::map_clone)]
        Ok(self.members.get(room_id).and_then(|m| m.get(state_key).map(|m| m.clone())))
    }

    fn get_user_ids(&self, room_id: &RoomId) -> Vec<UserId> {
        #[allow(clippy::map_clone)]
        self.members
            .get(room_id)
            .map(|u| u.iter().map(|u| u.key().clone()).collect())
            .unwrap_or_default()
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

    fn get_stripped_room_infos(&self) -> Vec<RoomInfo> {
        #[allow(clippy::map_clone)]
        self.stripped_room_info.iter().map(|r| r.clone()).collect()
    }

    async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        Ok(self.account_data.get(event_type.as_ref()).map(|e| e.clone()))
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        Ok(self
            .room_account_data
            .get(room_id)
            .and_then(|m| m.get(event_type.as_ref()).map(|e| e.clone())))
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(EventId, Receipt)>> {
        Ok(self.room_user_receipts.get(room_id).and_then(|m| {
            m.get(receipt_type.as_ref()).and_then(|m| m.get(user_id).map(|r| r.clone()))
        }))
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(UserId, Receipt)>> {
        Ok(self
            .room_event_receipts
            .get(room_id)
            .and_then(|m| {
                m.get(receipt_type.as_ref()).and_then(|m| {
                    m.get(event_id)
                        .map(|m| m.iter().map(|r| (r.key().clone(), r.value().clone())).collect())
                })
            })
            .unwrap_or_else(Vec::new))
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

    async fn get_timeline(
        &self,
        room_id: &RoomId,
        start: Option<&EventId>,
        end: Option<&EventId>,
        limit: Option<usize>,
        direction: Direction,
    ) -> Result<Option<StoredTimelineSlice>> {
        let event_id_to_timeline_slice = self.event_id_to_timeline_slice.get(room_id).unwrap();
        let timeline_slices = self.timeline_slices.get(room_id).unwrap();

        let slice = if let Some(start) = start {
            event_id_to_timeline_slice.get(start).and_then(|s| timeline_slices.get(&*s))
        } else {
            self.get_sync_token().await?.and_then(|s| timeline_slices.get(&*s))
        };

        let mut slice = if let Some(slice) = slice {
            slice
        } else {
            return Ok(None);
        };

        let mut timeline = StoredTimelineSlice::new(vec![], None);

        match direction {
            Direction::Forward => {
                loop {
                    timeline.events.append(&mut slice.events.iter().rev().cloned().collect());

                    timeline.token = Some(slice.start.clone());

                    let found_end = end.map_or(false, |end| {
                        slice.events.iter().any(|event| &event.event_id() == end)
                    });

                    if found_end {
                        break;
                    }

                    if let Some(next_slice) = timeline_slices.get(&slice.start) {
                        slice = next_slice;
                    } else {
                        // No more timeline slices or a gap in the known timeline
                        break;
                    }

                    if let Some(limit) = limit {
                        if timeline.events.len() >= limit {
                            break;
                        }
                    }
                }
            }
            Direction::Backward => {
                loop {
                    timeline.events.append(&mut slice.events.clone());
                    timeline.token = slice.end.clone();

                    let found_end = end.map_or(false, |end| {
                        slice.events.iter().any(|event| &event.event_id() == end)
                    });

                    if found_end {
                        break;
                    }

                    if let Some(prev_slice) =
                        slice.end.as_deref().and_then(|end| timeline_slices.get(end))
                    {
                        slice = prev_slice;
                    } else {
                        // No more timeline slices or we have a gap in the known timeline
                        break;
                    }

                    if let Some(limit) = limit {
                        if timeline.events.len() >= limit {
                            break;
                        }
                    }
                }
            }
        }
        Ok(Some(timeline))
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
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
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

    async fn get_user_ids(&self, room_id: &RoomId) -> Result<Vec<UserId>> {
        Ok(self.get_user_ids(room_id))
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

    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
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

    async fn get_account_data_event(
        &self,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        self.get_account_data_event(event_type).await
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: EventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.get_room_account_data_event(room_id, event_type).await
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        user_id: &UserId,
    ) -> Result<Option<(EventId, Receipt)>> {
        self.get_user_room_receipt_event(room_id, receipt_type, user_id).await
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        event_id: &EventId,
    ) -> Result<Vec<(UserId, Receipt)>> {
        self.get_event_room_receipt_events(room_id, receipt_type, event_id).await
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
    async fn get_timeline(
        &self,
        room_id: &RoomId,
        start: Option<&EventId>,
        end: Option<&EventId>,
        limit: Option<usize>,
        direction: Direction,
    ) -> Result<Option<StoredTimelineSlice>> {
        self.get_timeline(room_id, start, end, limit, direction).await
    }
}

#[cfg(test)]
#[cfg(not(feature = "sled_state_store"))]
mod test {
    use matrix_sdk_common::{
        api::client::r0::media::get_content_thumbnail::Method,
        identifiers::{event_id, mxc_uri, room_id, user_id, UserId},
        receipt::ReceiptType,
        uint,
    };
    use matrix_sdk_test::async_test;
    use serde_json::json;

    use super::{MemoryStore, StateChanges};
    use crate::media::{MediaFormat, MediaRequest, MediaThumbnailSize, MediaType};

    fn user_id() -> UserId {
        user_id!("@example:localhost")
    }

    #[async_test]
    async fn test_receipts_saving() {
        let store = MemoryStore::new();

        let room_id = room_id!("!test:localhost");

        let first_event_id = event_id!("$1435641916114394fHBLK:matrix.org");
        let second_event_id = event_id!("$fHBLK1435641916114394:matrix.org");

        let first_receipt_event = serde_json::from_value(json!({
            first_event_id.clone(): {
                "m.read": {
                    user_id(): {
                        "ts": 1436451550453u64
                    }
                }
            }
        }))
        .unwrap();

        let second_receipt_event = serde_json::from_value(json!({
            second_event_id.clone(): {
                "m.read": {
                    user_id(): {
                        "ts": 1436451551453u64
                    }
                }
            }
        }))
        .unwrap();

        assert!(store
            .get_user_room_receipt_event(&room_id, ReceiptType::Read, &user_id())
            .await
            .unwrap()
            .is_none());
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &first_event_id)
            .await
            .unwrap()
            .is_empty());
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &second_event_id)
            .await
            .unwrap()
            .is_empty());

        let mut changes = StateChanges::default();
        changes.add_receipts(&room_id, first_receipt_event);

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_user_room_receipt_event(&room_id, ReceiptType::Read, &user_id())
            .await
            .unwrap()
            .is_some(),);
        assert_eq!(
            store
                .get_event_room_receipt_events(&room_id, ReceiptType::Read, &first_event_id)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &second_event_id)
            .await
            .unwrap()
            .is_empty());

        let mut changes = StateChanges::default();
        changes.add_receipts(&room_id, second_receipt_event);

        store.save_changes(&changes).await.unwrap();
        assert!(store
            .get_user_room_receipt_event(&room_id, ReceiptType::Read, &user_id())
            .await
            .unwrap()
            .is_some());
        assert!(store
            .get_event_room_receipt_events(&room_id, ReceiptType::Read, &first_event_id)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .get_event_room_receipt_events(&room_id, ReceiptType::Read, &second_event_id)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[async_test]
    async fn test_media_content() {
        let store = MemoryStore::new();

        let uri = mxc_uri!("mxc://localhost/media");
        let content: Vec<u8> = "somebinarydata".into();

        let request_file =
            MediaRequest { media_type: MediaType::Uri(uri.clone()), format: MediaFormat::File };

        let request_thumbnail = MediaRequest {
            media_type: MediaType::Uri(uri.clone()),
            format: MediaFormat::Thumbnail(MediaThumbnailSize {
                method: Method::Crop,
                width: uint!(100),
                height: uint!(100),
            }),
        };

        assert!(store.get_media_content(&request_file).await.unwrap().is_none());
        assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none());

        store.add_media_content(&request_file, content.clone()).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_some());

        store.remove_media_content(&request_file).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_none());

        store.add_media_content(&request_file, content.clone()).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_some());

        store.add_media_content(&request_thumbnail, content.clone()).await.unwrap();
        assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_some());

        store.remove_media_content_for_uri(&uri).await.unwrap();
        assert!(store.get_media_content(&request_file).await.unwrap().is_none());
        assert!(store.get_media_content(&request_thumbnail).await.unwrap().is_none());
    }
}
