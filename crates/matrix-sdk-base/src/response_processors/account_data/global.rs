// Copyright 2024 The Matrix.org Foundation C.I.C.
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
    collections::{BTreeMap, HashMap, HashSet},
    mem,
};

use matrix_sdk_common::timer;
use ruma::{
    RoomId,
    events::{
        AnyGlobalAccountDataEvent, GlobalAccountDataEventType, direct::OwnedDirectUserIdentifier,
    },
    serde::Raw,
};
use tracing::{debug, instrument, trace, warn};

use super::super::Context;
use crate::{RoomInfo, StateChanges, store::BaseStateStore};

/// Create the [`Global`] account data processor.
pub fn global(events: &[Raw<AnyGlobalAccountDataEvent>]) -> Global {
    Global::process(events)
}

#[must_use]
pub struct Global {
    parsed_events: Vec<AnyGlobalAccountDataEvent>,
    raw_by_type: BTreeMap<GlobalAccountDataEventType, Raw<AnyGlobalAccountDataEvent>>,
}

impl Global {
    /// Creates a new processor for global account data.
    fn process(events: &[Raw<AnyGlobalAccountDataEvent>]) -> Self {
        let _timer = timer!(tracing::Level::TRACE, "Global::process (global account data)");

        let mut raw_by_type = BTreeMap::new();
        let mut parsed_events = Vec::new();

        for raw_event in events {
            let event = match raw_event.deserialize() {
                Ok(e) => e,
                Err(e) => {
                    let event_type: Option<String> = raw_event.get_field("type").ok().flatten();
                    warn!(event_type, "Failed to deserialize a global account data event: {e}");
                    continue;
                }
            };

            raw_by_type.insert(event.event_type(), raw_event.clone());
            parsed_events.push(event);
        }

        Self { raw_by_type, parsed_events }
    }

    /// Returns the push rules found by this processor.
    pub fn push_rules(&self) -> Option<&Raw<AnyGlobalAccountDataEvent>> {
        self.raw_by_type.get(&GlobalAccountDataEventType::PushRules)
    }

    /// Processes the direct rooms in a sync response:
    ///
    /// Given a [`StateChanges`] instance, processes any direct room info
    /// from the global account data and adds it to the room infos to
    /// save.
    #[instrument(skip_all)]
    fn process_direct_rooms(
        &self,
        events: &[AnyGlobalAccountDataEvent],
        state_store: &BaseStateStore,
        state_changes: &mut StateChanges,
    ) {
        for event in events {
            let AnyGlobalAccountDataEvent::Direct(direct_event) = event else { continue };

            let mut new_dms = HashMap::<&RoomId, HashSet<OwnedDirectUserIdentifier>>::new();

            for (user_identifier, rooms) in direct_event.content.iter() {
                for room_id in rooms {
                    new_dms.entry(room_id).or_default().insert(user_identifier.clone());
                }
            }

            let rooms = state_store.rooms();
            let mut old_dms = rooms
                .iter()
                .filter_map(|r| {
                    let direct_targets = r.direct_targets();
                    (!direct_targets.is_empty()).then(|| (r.room_id(), direct_targets))
                })
                .collect::<HashMap<_, _>>();

            // Update the direct targets of rooms if they changed.
            for (room_id, new_direct_targets) in new_dms {
                if let Some(old_direct_targets) = old_dms.remove(&room_id)
                    && old_direct_targets == new_direct_targets
                {
                    continue;
                }
                trace!(?room_id, targets = ?new_direct_targets, "Marking room as direct room");
                map_info(room_id, state_changes, state_store, |info| {
                    info.base_info.dm_targets = new_direct_targets;
                });
            }

            // Remove the targets of old direct chats.
            for room_id in old_dms.keys() {
                trace!(?room_id, "Unmarking room as direct room");
                map_info(room_id, state_changes, state_store, |info| {
                    info.base_info.dm_targets.clear();
                });
            }
        }
    }

    /// Applies the processed data to the state changes and the state store.
    pub async fn apply(mut self, context: &mut Context, state_store: &BaseStateStore) {
        let _timer = timer!(tracing::Level::TRACE, "Global::apply (global account data)");

        // Fill in the content of `changes.account_data`.
        mem::swap(&mut context.state_changes.account_data, &mut self.raw_by_type);

        // Process direct rooms.
        let has_new_direct_room_data = self
            .parsed_events
            .iter()
            .any(|event| event.event_type() == GlobalAccountDataEventType::Direct);

        if has_new_direct_room_data {
            self.process_direct_rooms(&self.parsed_events, state_store, &mut context.state_changes);
        } else if let Ok(Some(direct_account_data)) =
            state_store.get_account_data_event(GlobalAccountDataEventType::Direct).await
        {
            debug!("Found direct room data in the Store, applying it");
            if let Ok(direct_account_data) = direct_account_data.deserialize() {
                self.process_direct_rooms(
                    &[direct_account_data],
                    state_store,
                    &mut context.state_changes,
                );
            } else {
                warn!("Failed to deserialize direct room account data");
            }
        }
    }
}

/// Applies a function to an existing `RoomInfo` if present in changes, or one
/// loaded from the database.
fn map_info<F: FnOnce(&mut RoomInfo)>(
    room_id: &RoomId,
    changes: &mut StateChanges,
    store: &BaseStateStore,
    f: F,
) {
    if let Some(info) = changes.room_infos.get_mut(room_id) {
        f(info);
    } else if let Some(room) = store.room(room_id) {
        let mut info = room.clone_info();
        f(&mut info);
        changes.add_room(info);
    } else if store.already_logged_missing_room.lock().insert(room_id.to_owned()) {
        debug!(room = %room_id, "couldn't find room in state changes or store");
    }
}
