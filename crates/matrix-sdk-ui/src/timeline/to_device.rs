// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::iter;

use matrix_sdk::event_handler::EventHandler;
use ruma::{
    OwnedRoomId,
    events::{forwarded_room_key::ToDeviceForwardedRoomKeyEvent, room_key::ToDeviceRoomKeyEvent},
};
use tracing::{Instrument, debug_span, trace};

use super::controller::TimelineController;

pub(super) fn handle_room_key_event(
    timeline: TimelineController,
    room_id: OwnedRoomId,
) -> impl EventHandler<ToDeviceRoomKeyEvent, ()> {
    move |event: ToDeviceRoomKeyEvent| {
        async move {
            let event_room_id = event.content.room_id;
            let session_id = event.content.session_id;
            retry_decryption(timeline, room_id, event_room_id, session_id).await;
        }
        .instrument(debug_span!("handle_room_key_event"))
    }
}

pub(super) fn handle_forwarded_room_key_event(
    timeline: TimelineController,
    room_id: OwnedRoomId,
) -> impl EventHandler<ToDeviceForwardedRoomKeyEvent, ()> {
    move |event: ToDeviceForwardedRoomKeyEvent| {
        async move {
            let event_room_id = event.content.room_id;
            let session_id = event.content.session_id;
            retry_decryption(timeline, room_id, event_room_id, session_id).await;
        }
        .instrument(debug_span!("handle_forwarded_room_key_event"))
    }
}

async fn retry_decryption(
    timeline: TimelineController,
    room_id: OwnedRoomId,
    event_room_id: OwnedRoomId,
    session_id: String,
) {
    if event_room_id != room_id {
        trace!(
            ?event_room_id, timeline_room_id = ?room_id, ?session_id,
            "Received to-device room key event for a different room, ignoring"
        );
        return;
    }

    timeline.retry_event_decryption(Some(iter::once(session_id).collect())).await;
}
