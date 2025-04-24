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

use super::super::Context;
use crate::{
    rooms::UpdatedRoomDisplayName, store::BaseStateStore, sync::RoomUpdates,
    RoomInfoNotableUpdateReasons,
};

pub async fn update_for_rooms(
    context: &mut Context,
    room_updates: &RoomUpdates,
    state_store: &BaseStateStore,
) {
    for room in room_updates
        .left
        .keys()
        .chain(room_updates.joined.keys())
        .chain(room_updates.invited.keys())
        .chain(room_updates.knocked.keys())
        .filter_map(|room_id| state_store.room(room_id))
    {
        // Compute the display name. If it's different, let's register the `RoomInfo` in
        // the `StateChanges`.
        if let Ok(UpdatedRoomDisplayName::New(_)) = room.compute_display_name().await {
            let room_id = room.room_id().to_owned();

            context.state_changes.room_infos.insert(room_id.clone(), room.clone_info());
            context
                .room_info_notable_updates
                .entry(room_id)
                .or_default()
                .insert(RoomInfoNotableUpdateReasons::DISPLAY_NAME);
        }
    }
}
