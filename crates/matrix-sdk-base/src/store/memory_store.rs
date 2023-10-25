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
    sync::RwLock as StdRwLock,
};

use async_trait::async_trait;
use matrix_sdk_common::instant::Instant;
use ruma::{
    canonical_json::{redact, RedactedBecause},
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
#[derive(Debug, Default)]
pub struct MemoryStore {
    user_avatar_url: StdRwLock<HashMap<String, String>>,
    sync_token: StdRwLock<Option<String>>,
    filters: StdRwLock<HashMap<String, String>>,
    account_data: StdRwLock<HashMap<GlobalAccountDataEventType, Raw<AnyGlobalAccountDataEvent>>>,
    profiles: StdRwLock<HashMap<OwnedRoomId, HashMap<OwnedUserId, MinimalRoomMemberEvent>>>,
    display_names: StdRwLock<HashMap<OwnedRoomId, HashMap<String, BTreeSet<OwnedUserId>>>>,
    members: StdRwLock<HashMap<OwnedRoomId, HashMap<OwnedUserId, MembershipState>>>,
    room_info: StdRwLock<HashMap<OwnedRoomId, RoomInfo>>,
    room_state: StdRwLock<
        HashMap<OwnedRoomId, HashMap<StateEventType, HashMap<String, Raw<AnySyncStateEvent>>>>,
    >,
    room_account_data: StdRwLock<
        HashMap<OwnedRoomId, HashMap<RoomAccountDataEventType, Raw<AnyRoomAccountDataEvent>>>,
    >,
    stripped_room_state: StdRwLock<
        HashMap<OwnedRoomId, HashMap<StateEventType, HashMap<String, Raw<AnyStrippedStateEvent>>>>,
    >,
    stripped_members: StdRwLock<HashMap<OwnedRoomId, HashMap<OwnedUserId, MembershipState>>>,
    presence: StdRwLock<HashMap<OwnedUserId, Raw<PresenceEvent>>>,
    room_user_receipts: StdRwLock<
        HashMap<
            OwnedRoomId,
            HashMap<(String, Option<String>), HashMap<OwnedUserId, (OwnedEventId, Receipt)>>,
        >,
    >,
    room_event_receipts: StdRwLock<
        HashMap<
            OwnedRoomId,
            HashMap<(String, Option<String>), HashMap<OwnedEventId, HashMap<OwnedUserId, Receipt>>>,
        >,
    >,
    custom: StdRwLock<HashMap<Vec<u8>, Vec<u8>>>,
}

impl MemoryStore {
    /// Create a new empty MemoryStore
    pub fn new() -> Self {
        Default::default()
    }

    fn get_user_room_receipt_event_impl(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        self.room_user_receipts
            .read()
            .unwrap()
            .get(room_id)?
            .get(&(receipt_type.to_string(), thread.as_str().map(ToOwned::to_owned)))?
            .get(user_id)
            .cloned()
    }

    fn get_event_room_receipt_events_impl(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Option<Vec<(OwnedUserId, Receipt)>> {
        Some(
            self.room_event_receipts
                .read()
                .unwrap()
                .get(room_id)?
                .get(&(receipt_type.to_string(), thread.as_str().map(ToOwned::to_owned)))?
                .get(event_id)?
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        )
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl StateStore for MemoryStore {
    type Error = StoreError;

    async fn get_kv_data(&self, key: StateStoreDataKey<'_>) -> Result<Option<StateStoreDataValue>> {
        Ok(match key {
            StateStoreDataKey::SyncToken => {
                self.sync_token.read().unwrap().clone().map(StateStoreDataValue::SyncToken)
            }
            StateStoreDataKey::Filter(filter_name) => self
                .filters
                .read()
                .unwrap()
                .get(filter_name)
                .cloned()
                .map(StateStoreDataValue::Filter),
            StateStoreDataKey::UserAvatarUrl(user_id) => self
                .user_avatar_url
                .read()
                .unwrap()
                .get(user_id.as_str())
                .cloned()
                .map(StateStoreDataValue::UserAvatarUrl),
        })
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
                self.filters.write().unwrap().insert(
                    filter_name.to_owned(),
                    value.into_filter().expect("Session data not a filter"),
                );
            }
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                self.filters.write().unwrap().insert(
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
                self.filters.write().unwrap().remove(filter_name);
            }
            StateStoreDataKey::UserAvatarUrl(user_id) => {
                self.filters.write().unwrap().remove(user_id.as_str());
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
                    .write()
                    .unwrap()
                    .entry(room.clone())
                    .or_default()
                    .insert(user_id.clone(), profile.clone());
            }
        }

        for (room, map) in &changes.ambiguity_maps {
            for (display_name, display_names) in map {
                self.display_names
                    .write()
                    .unwrap()
                    .entry(room.clone())
                    .or_default()
                    .insert(display_name.clone(), display_names.clone());
            }
        }

        {
            let mut account_data = self.account_data.write().unwrap();
            for (event_type, event) in &changes.account_data {
                account_data.insert(event_type.clone(), event.clone());
            }
        }

        {
            let mut room_account_data = self.room_account_data.write().unwrap();
            for (room, events) in &changes.room_account_data {
                for (event_type, event) in events {
                    room_account_data
                        .entry(room.clone())
                        .or_default()
                        .insert(event_type.clone(), event.clone());
                }
            }
        }

        {
            let mut room_state = self.room_state.write().unwrap();
            let mut stripped_room_state = self.stripped_room_state.write().unwrap();
            let mut members = self.members.write().unwrap();
            let mut stripped_members = self.stripped_members.write().unwrap();

            for (room, event_types) in &changes.state {
                for (event_type, events) in event_types {
                    for (state_key, raw_event) in events {
                        room_state
                            .entry(room.clone())
                            .or_default()
                            .entry(event_type.clone())
                            .or_default()
                            .insert(state_key.to_owned(), raw_event.clone());
                        stripped_room_state.remove(room);

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

                            stripped_members.remove(room);

                            members
                                .entry(room.clone())
                                .or_default()
                                .insert(event.state_key().to_owned(), event.membership().clone());
                        }
                    }
                }
            }
        }

        {
            let mut room_info = self.room_info.write().unwrap();
            for (room_id, info) in &changes.room_infos {
                room_info.insert(room_id.clone(), info.clone());
            }
        }

        {
            let mut presence = self.presence.write().unwrap();
            for (sender, event) in &changes.presence {
                presence.insert(sender.clone(), event.clone());
            }
        }

        {
            let mut stripped_room_state = self.stripped_room_state.write().unwrap();
            let mut stripped_members = self.stripped_members.write().unwrap();

            for (room, event_types) in &changes.stripped_state {
                for (event_type, events) in event_types {
                    for (state_key, raw_event) in events {
                        stripped_room_state
                            .entry(room.clone())
                            .or_default()
                            .entry(event_type.clone())
                            .or_default()
                            .insert(state_key.to_owned(), raw_event.clone());

                        if *event_type == StateEventType::RoomMember {
                            let event = match raw_event.deserialize_as::<StrippedRoomMemberEvent>()
                            {
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

                            stripped_members
                                .entry(room.clone())
                                .or_default()
                                .insert(event.state_key, event.content.membership.clone());
                        }
                    }
                }
            }
        }

        {
            let mut room_user_receipts = self.room_user_receipts.write().unwrap();
            let mut room_event_receipts = self.room_event_receipts.write().unwrap();

            for (room, content) in &changes.receipts {
                for (event_id, receipts) in &content.0 {
                    for (receipt_type, receipts) in receipts {
                        for (user_id, receipt) in receipts {
                            let thread = receipt.thread.as_str().map(ToOwned::to_owned);
                            // Add the receipt to the room user receipts
                            if let Some((old_event, _)) = room_user_receipts
                                .entry(room.clone())
                                .or_default()
                                .entry((receipt_type.to_string(), thread.clone()))
                                .or_default()
                                .insert(user_id.clone(), (event_id.clone(), receipt.clone()))
                            {
                                // Remove the old receipt from the room event receipts
                                if let Some(receipt_map) = room_event_receipts.get_mut(room) {
                                    if let Some(event_map) = receipt_map
                                        .get_mut(&(receipt_type.to_string(), thread.clone()))
                                    {
                                        if let Some(user_map) = event_map.get_mut(&old_event) {
                                            user_map.remove(user_id);
                                        }
                                    }
                                }
                            }

                            // Add the receipt to the room event receipts
                            room_event_receipts
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
        }

        {
            let room_info = self.room_info.read().unwrap();
            let mut room_state = self.room_state.write().unwrap();

            let make_room_version = |room_id| {
                room_info.get(room_id).and_then(|info| info.room_version().cloned()).unwrap_or_else(
                    || {
                        warn!(?room_id, "Unable to find the room version, assuming version 9");
                        RoomVersionId::V9
                    },
                )
            };

            for (room_id, redactions) in &changes.redactions {
                let mut room_version = None;
                if let Some(room) = room_state.get_mut(room_id) {
                    for ref_room_mu in room.values_mut() {
                        for raw_evt in ref_room_mu.values_mut() {
                            if let Ok(Some(event_id)) =
                                raw_evt.get_field::<OwnedEventId>("event_id")
                            {
                                if let Some(redaction) = redactions.get(&event_id) {
                                    let redacted = redact(
                                        raw_evt.deserialize_as::<CanonicalJsonObject>()?,
                                        room_version
                                            .get_or_insert_with(|| make_room_version(room_id)),
                                        Some(RedactedBecause::from_raw_event(redaction)?),
                                    )
                                    .map_err(StoreError::Redaction)?;
                                    *raw_evt = Raw::new(&redacted)?.cast();
                                }
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
        Ok(self.presence.read().unwrap().get(user_id).cloned())
    }

    async fn get_presence_events(
        &self,
        user_ids: &[OwnedUserId],
    ) -> Result<Vec<Raw<PresenceEvent>>> {
        let presence = self.presence.read().unwrap();
        Ok(user_ids.iter().filter_map(|user_id| presence.get(user_id).cloned()).collect())
    }

    async fn get_state_event(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<RawAnySyncOrStrippedState>> {
        Ok(self
            .get_state_events_for_keys(room_id, event_type, &[state_key])
            .await?
            .into_iter()
            .next())
    }

    async fn get_state_events(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        fn get_events<T>(
            state_map: &HashMap<OwnedRoomId, HashMap<StateEventType, HashMap<String, Raw<T>>>>,
            room_id: &RoomId,
            event_type: &StateEventType,
            to_enum: fn(Raw<T>) -> RawAnySyncOrStrippedState,
        ) -> Option<Vec<RawAnySyncOrStrippedState>> {
            let state_events = state_map.get(room_id)?.get(event_type)?;
            Some(state_events.values().cloned().map(to_enum).collect())
        }

        Ok(get_events(
            &self.stripped_room_state.read().unwrap(),
            room_id,
            &event_type,
            RawAnySyncOrStrippedState::Stripped,
        )
        .or_else(|| {
            get_events(
                &self.room_state.read().unwrap(),
                room_id,
                &event_type,
                RawAnySyncOrStrippedState::Sync,
            )
        })
        .unwrap_or_default())
    }

    async fn get_state_events_for_keys(
        &self,
        room_id: &RoomId,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>, Self::Error> {
        Ok(
            if let Some(stripped_state_events) = self
                .stripped_room_state
                .read()
                .unwrap()
                .get(room_id)
                .and_then(|events| events.get(&event_type))
            {
                state_keys
                    .iter()
                    .filter_map(|k| {
                        stripped_state_events
                            .get(*k)
                            .map(|e| RawAnySyncOrStrippedState::Stripped(e.clone()))
                    })
                    .collect()
            } else if let Some(sync_state_events) = self
                .room_state
                .read()
                .unwrap()
                .get(room_id)
                .and_then(|events| events.get(&event_type))
            {
                state_keys
                    .iter()
                    .filter_map(|k| {
                        sync_state_events
                            .get(*k)
                            .map(|e| RawAnySyncOrStrippedState::Sync(e.clone()))
                    })
                    .collect()
            } else {
                Vec::new()
            },
        )
    }

    async fn get_profile(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<MinimalRoomMemberEvent>> {
        Ok(self
            .profiles
            .read()
            .unwrap()
            .get(room_id)
            .and_then(|room_profiles| room_profiles.get(user_id))
            .cloned())
    }

    async fn get_profiles<'a>(
        &self,
        room_id: &RoomId,
        user_ids: &'a [OwnedUserId],
    ) -> Result<BTreeMap<&'a UserId, MinimalRoomMemberEvent>> {
        if user_ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let profiles = self.profiles.read().unwrap();
        let Some(room_profiles) = profiles.get(room_id) else {
            return Ok(BTreeMap::new());
        };

        Ok(user_ids
            .iter()
            .filter_map(|user_id| room_profiles.get(user_id).map(|p| (&**user_id, p.clone())))
            .collect())
    }

    async fn get_user_ids(
        &self,
        room_id: &RoomId,
        memberships: RoomMemberships,
    ) -> Result<Vec<OwnedUserId>> {
        /// Get the user IDs for the given room with the given memberships and
        /// stripped state.
        ///
        /// If `memberships` is empty, returns all user IDs in the room with the
        /// given stripped state.
        fn get_user_ids_inner(
            members: &HashMap<OwnedRoomId, HashMap<OwnedUserId, MembershipState>>,
            room_id: &RoomId,
            memberships: RoomMemberships,
        ) -> Vec<OwnedUserId> {
            members
                .get(room_id)
                .map(|members| {
                    members
                        .iter()
                        .filter_map(|(user_id, membership)| {
                            memberships.matches(membership).then_some(user_id)
                        })
                        .cloned()
                        .collect()
                })
                .unwrap_or_default()
        }

        let v = get_user_ids_inner(&self.stripped_members.read().unwrap(), room_id, memberships);
        if !v.is_empty() {
            return Ok(v);
        }
        Ok(get_user_ids_inner(&self.members.read().unwrap(), room_id, memberships))
    }

    async fn get_invited_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        StateStore::get_user_ids(self, room_id, RoomMemberships::INVITE).await
    }

    async fn get_joined_user_ids(&self, room_id: &RoomId) -> Result<Vec<OwnedUserId>> {
        StateStore::get_user_ids(self, room_id, RoomMemberships::JOIN).await
    }

    async fn get_room_infos(&self) -> Result<Vec<RoomInfo>> {
        Ok(self.room_info.read().unwrap().values().cloned().collect())
    }

    async fn get_stripped_room_infos(&self) -> Result<Vec<RoomInfo>> {
        Ok(self
            .room_info
            .read()
            .unwrap()
            .values()
            .filter(|r| matches!(r.state(), RoomState::Invited))
            .cloned()
            .collect())
    }

    async fn get_users_with_display_name(
        &self,
        room_id: &RoomId,
        display_name: &str,
    ) -> Result<BTreeSet<OwnedUserId>> {
        Ok(self
            .display_names
            .read()
            .unwrap()
            .get(room_id)
            .and_then(|room_names| room_names.get(display_name).cloned())
            .unwrap_or_default())
    }

    async fn get_users_with_display_names<'a>(
        &self,
        room_id: &RoomId,
        display_names: &'a [String],
    ) -> Result<BTreeMap<&'a str, BTreeSet<OwnedUserId>>> {
        if display_names.is_empty() {
            return Ok(BTreeMap::new());
        }

        let read_guard = &self.display_names.read().unwrap();
        let Some(room_names) = read_guard.get(room_id) else {
            return Ok(BTreeMap::new());
        };

        Ok(display_names
            .iter()
            .filter_map(|n| room_names.get(n).map(|d| (n.as_str(), d.clone())))
            .collect())
    }

    async fn get_account_data_event(
        &self,
        event_type: GlobalAccountDataEventType,
    ) -> Result<Option<Raw<AnyGlobalAccountDataEvent>>> {
        Ok(self.account_data.read().unwrap().get(&event_type).cloned())
    }

    async fn get_room_account_data_event(
        &self,
        room_id: &RoomId,
        event_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        Ok(self
            .room_account_data
            .read()
            .unwrap()
            .get(room_id)
            .and_then(|m| m.get(&event_type))
            .cloned())
    }

    async fn get_user_room_receipt_event(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        Ok(self.get_user_room_receipt_event_impl(room_id, receipt_type, thread, user_id))
    }

    async fn get_event_room_receipt_events(
        &self,
        room_id: &RoomId,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        Ok(self
            .get_event_room_receipt_events_impl(room_id, receipt_type, thread, event_id)
            .unwrap_or_default())
    }

    async fn get_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.custom.read().unwrap().get(key).cloned())
    }

    async fn set_custom_value(&self, key: &[u8], value: Vec<u8>) -> Result<Option<Vec<u8>>> {
        Ok(self.custom.write().unwrap().insert(key.to_vec(), value))
    }

    async fn remove_custom_value(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.custom.write().unwrap().remove(key))
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
        self.profiles.write().unwrap().remove(room_id);
        self.display_names.write().unwrap().remove(room_id);
        self.members.write().unwrap().remove(room_id);
        self.room_info.write().unwrap().remove(room_id);
        self.room_state.write().unwrap().remove(room_id);
        self.room_account_data.write().unwrap().remove(room_id);
        self.stripped_room_state.write().unwrap().remove(room_id);
        self.stripped_members.write().unwrap().remove(room_id);
        self.room_user_receipts.write().unwrap().remove(room_id);
        self.room_event_receipts.write().unwrap().remove(room_id);

        Ok(())
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
