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

use ruma::{RoomId, events::AnySyncEphemeralRoomEvent, serde::Raw};
use tracing::info;

use super::Context;

/// Dispatch [`AnySyncEphemeralRoomEvent`]s on the [`Context`].
pub fn dispatch(
    context: &mut Context,
    raw_events: &[Raw<AnySyncEphemeralRoomEvent>],
    room_id: &RoomId,
) {
    for raw_event in raw_events {
        dispatch_receipt(context, raw_event, room_id);
    }
}

/// Dispatch the [`AnySyncEphemeralRoomEvent::Receipt`] on the [`Context`].
pub(super) fn dispatch_receipt(
    context: &mut Context,
    raw_event: &Raw<AnySyncEphemeralRoomEvent>,
    room_id: &RoomId,
) {
    match raw_event.deserialize() {
        Ok(AnySyncEphemeralRoomEvent::Receipt(event)) => {
            context.state_changes.add_receipts(room_id, event.content);
        }

        Ok(_) => {}

        Err(e) => {
            let event_id = raw_event.get_field::<String>("event_id").ok().flatten();

            info!(?room_id, event_id, "Failed to deserialize ephemeral room event: {e}");
        }
    }
}
