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

use ruma::{
    RoomId,
    events::{AnyRoomAccountDataEvent, marked_unread::MarkedUnreadEventContent},
    serde::Raw,
};
use tracing::{instrument, warn};

use super::super::{Context, RoomInfoNotableUpdates};
use crate::{
    RoomInfo, RoomInfoNotableUpdateReasons, StateChanges, room::AccountDataSource,
    store::BaseStateStore,
};

#[instrument(skip_all, fields(?room_id))]
pub fn for_room(
    context: &mut Context,
    room_id: &RoomId,
    events: &[Raw<AnyRoomAccountDataEvent>],
    state_store: &BaseStateStore,
) {
    // Handle new events.
    for raw_event in events {
        match raw_event.deserialize() {
            Ok(event) => {
                context.state_changes.add_room_account_data(
                    room_id,
                    event.clone(),
                    raw_event.clone(),
                );

                match event {
                    AnyRoomAccountDataEvent::MarkedUnread(event) => {
                        on_room_info(
                            room_id,
                            &mut context.state_changes,
                            state_store,
                            |room_info| {
                                on_unread_marker(
                                    room_id,
                                    &event.content,
                                    AccountDataSource::Stable,
                                    room_info,
                                    &mut context.room_info_notable_updates,
                                );
                            },
                        );
                    }
                    AnyRoomAccountDataEvent::UnstableMarkedUnread(event) => {
                        on_room_info(
                            room_id,
                            &mut context.state_changes,
                            state_store,
                            |room_info| {
                                on_unread_marker(
                                    room_id,
                                    &event.content.0,
                                    AccountDataSource::Unstable,
                                    room_info,
                                    &mut context.room_info_notable_updates,
                                );
                            },
                        );
                    }
                    AnyRoomAccountDataEvent::Tag(event) => {
                        on_room_info(
                            room_id,
                            &mut context.state_changes,
                            state_store,
                            |room_info| {
                                room_info.base_info.handle_notable_tags(&event.content.tags);
                            },
                        );
                    }

                    // Nothing.
                    _ => {}
                }
            }

            Err(err) => {
                warn!("unable to deserialize account data event: {err}");
            }
        }
    }
}

// Small helper to make the code easier to read.
//
// It finds the appropriate `RoomInfo`, allowing the caller to modify it, and
// save it in the correct place.
fn on_room_info<F>(
    room_id: &RoomId,
    state_changes: &mut StateChanges,
    state_store: &BaseStateStore,
    mut on_room_info: F,
) where
    F: FnMut(&mut RoomInfo),
{
    // `StateChanges` has the `RoomInfo`.
    if let Some(room_info) = state_changes.room_infos.get_mut(room_id) {
        // Show time.
        on_room_info(room_info);
    }
    // The `BaseStateStore` has the `Room`, which has the `RoomInfo`.
    else if let Some(room) = state_store.room(room_id) {
        // Clone the `RoomInfo`.
        let mut room_info = room.clone_info();

        // Show time.
        on_room_info(&mut room_info);

        // Update the `RoomInfo` via `StateChanges`.
        state_changes.add_room(room_info);
    }
}

// Helper to update the unread marker for stable and unstable prefixes.
fn on_unread_marker(
    room_id: &RoomId,
    content: &MarkedUnreadEventContent,
    source: AccountDataSource,
    room_info: &mut RoomInfo,
    room_info_notable_updates: &mut RoomInfoNotableUpdates,
) {
    if room_info.base_info.is_marked_unread_source == AccountDataSource::Stable
        && source != AccountDataSource::Stable
    {
        // Ignore the unstable source if a stable source was used previously.
        return;
    }

    if room_info.base_info.is_marked_unread != content.unread {
        // Notify the room list about a manual read marker change if the
        // value's changed.
        room_info_notable_updates
            .entry(room_id.to_owned())
            .or_default()
            .insert(RoomInfoNotableUpdateReasons::UNREAD_MARKER);
    }

    room_info.base_info.is_marked_unread = content.unread;
    room_info.base_info.is_marked_unread_source = source;
}
