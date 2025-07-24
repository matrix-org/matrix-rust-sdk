// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use std::collections::BTreeMap;

use eyeball::{AsyncLock, ObservableWriteGuard};
use ruma::{
    OwnedEventId, OwnedUserId,
    events::{
        StateEventType, SyncStateEvent,
        room::member::{MembershipState, RoomMemberEventContent},
    },
};
use tracing::warn;

use super::Room;
use crate::{
    StateStoreDataKey, StateStoreDataValue, StoreError,
    deserialized_responses::{MemberEvent, RawMemberEvent, SyncOrStrippedState},
    store::{Result as StoreResult, StateStoreExt},
};

impl Room {
    /// Mark a list of requests to join the room as seen, given their state
    /// event ids.
    pub async fn mark_knock_requests_as_seen(&self, user_ids: &[OwnedUserId]) -> StoreResult<()> {
        let raw_user_ids: Vec<&str> = user_ids.iter().map(|id| id.as_str()).collect();
        let member_raw_events = self
            .store
            .get_state_events_for_keys(self.room_id(), StateEventType::RoomMember, &raw_user_ids)
            .await?;
        let mut event_to_user_ids = Vec::with_capacity(member_raw_events.len());

        // Map the list of events ids to their user ids, if they are event ids for knock
        // membership events. Log an error and continue otherwise.
        for raw_event in member_raw_events {
            let event = raw_event.cast::<RoomMemberEventContent>().deserialize()?;
            match event {
                SyncOrStrippedState::Sync(SyncStateEvent::Original(event)) => {
                    if event.content.membership == MembershipState::Knock {
                        event_to_user_ids.push((event.event_id, event.state_key))
                    } else {
                        warn!(
                            "Could not mark knock event as seen: event {} for user {} \
                             is not in Knock membership state.",
                            event.event_id, event.state_key
                        );
                    }
                }
                _ => warn!(
                    "Could not mark knock event as seen: event for user {} is not valid.",
                    event.state_key()
                ),
            }
        }

        let current_seen_events_guard = self.get_write_guarded_current_knock_request_ids().await?;
        let mut current_seen_events = current_seen_events_guard.clone().unwrap_or_default();

        current_seen_events.extend(event_to_user_ids);

        self.update_seen_knock_request_ids(current_seen_events_guard, current_seen_events).await?;

        Ok(())
    }

    /// Removes the seen knock request ids that are no longer valid given the
    /// current room members.
    pub async fn remove_outdated_seen_knock_requests_ids(&self) -> StoreResult<()> {
        let current_seen_events_guard = self.get_write_guarded_current_knock_request_ids().await?;
        let mut current_seen_events = current_seen_events_guard.clone().unwrap_or_default();

        // Get and deserialize the member events for the seen knock requests
        let keys: Vec<OwnedUserId> = current_seen_events.values().map(|id| id.to_owned()).collect();
        let raw_member_events: Vec<RawMemberEvent> =
            self.store.get_state_events_for_keys_static(self.room_id(), &keys).await?;
        let member_events = raw_member_events
            .into_iter()
            .map(|raw| raw.deserialize())
            .collect::<Result<Vec<MemberEvent>, _>>()?;

        let mut ids_to_remove = Vec::new();

        for (event_id, user_id) in current_seen_events.iter() {
            // Check the seen knock request ids against the current room member events for
            // the room members associated to them
            let matching_member = member_events.iter().find(|event| event.user_id() == user_id);

            if let Some(member) = matching_member {
                let member_event_id = member.event_id();
                // If the member event is not a knock or it's different knock, it's outdated
                if *member.membership() != MembershipState::Knock
                    || member_event_id.is_some_and(|id| id != event_id)
                {
                    ids_to_remove.push(event_id.to_owned());
                }
            } else {
                ids_to_remove.push(event_id.to_owned());
            }
        }

        // If there are no ids to remove, do nothing
        if ids_to_remove.is_empty() {
            return Ok(());
        }

        for event_id in ids_to_remove {
            current_seen_events.remove(&event_id);
        }

        self.update_seen_knock_request_ids(current_seen_events_guard, current_seen_events).await?;

        Ok(())
    }

    /// Get the list of seen knock request event ids in this room.
    pub async fn get_seen_knock_request_ids(
        &self,
    ) -> Result<BTreeMap<OwnedEventId, OwnedUserId>, StoreError> {
        Ok(self.get_write_guarded_current_knock_request_ids().await?.clone().unwrap_or_default())
    }

    async fn get_write_guarded_current_knock_request_ids(
        &self,
    ) -> StoreResult<ObservableWriteGuard<'_, Option<BTreeMap<OwnedEventId, OwnedUserId>>, AsyncLock>>
    {
        let mut guard = self.seen_knock_request_ids_map.write().await;
        // If there are no loaded request ids yet
        if guard.is_none() {
            // Load the values from the store and update the shared observable contents
            let updated_seen_ids = self
                .store
                .get_kv_data(StateStoreDataKey::SeenKnockRequests(self.room_id()))
                .await?
                .and_then(|v| v.into_seen_knock_requests())
                .unwrap_or_default();

            ObservableWriteGuard::set(&mut guard, Some(updated_seen_ids));
        }
        Ok(guard)
    }

    async fn update_seen_knock_request_ids(
        &self,
        mut guard: ObservableWriteGuard<'_, Option<BTreeMap<OwnedEventId, OwnedUserId>>, AsyncLock>,
        new_value: BTreeMap<OwnedEventId, OwnedUserId>,
    ) -> StoreResult<()> {
        // Save the new values to the shared observable
        ObservableWriteGuard::set(&mut guard, Some(new_value.clone()));

        // Save them into the store too
        self.store
            .set_kv_data(
                StateStoreDataKey::SeenKnockRequests(self.room_id()),
                StateStoreDataValue::SeenKnockRequests(new_value),
            )
            .await?;

        Ok(())
    }
}
