// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Automatic mirroring of this device's MatrixRTC participation into the
//! [MSC4426] `m.call` profile field.
//!
//! [MSC4426]: https://github.com/matrix-org/matrix-spec-proposals/pull/4426

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use matrix_sdk_common::executor::spawn;
use ruma::{
    OwnedRoomId, SecondsSinceUnixEpoch,
    events::{OriginalSyncStateEvent, call::member::CallMemberEventContent},
};
use tracing::warn;

use crate::{Client, Room, client::WeakClient, event_handler::EventHandlerHandle};

/// Owns the `m.call.member` event handler for auto-syncing the `m.call`
/// profile field. Dropping this struct deregisters the handler.
/// Holds a [`WeakClient`] rather than a strong `Client` to avoid a
/// reference cycle.
pub(crate) struct AutomaticCallStatus {
    handle: EventHandlerHandle,
    client: WeakClient,
}

/// Rooms in which this device is currently participating in an active
/// MatrixRTC call. Maintained incrementally from `m.call.member` events.
type ActiveCallRooms = Arc<Mutex<HashSet<OwnedRoomId>>>;

impl Client {
    /// Enable or disable automatic mirroring of this device's MatrixRTC
    /// participation into the [MSC4426] `m.call` profile field. Off by
    /// default.
    ///
    /// Toggling `false -> true` registers a typed event handler for
    /// `m.call.member` state events. Toggling `true -> false` deregisters
    /// it.
    ///
    /// Toggling `true -> false` does NOT clear `m.call` on the server, you
    /// should call [`crate::Account::clear_call`] explicitly if that is
    /// desired.
    ///
    /// [MSC4426]: https://github.com/matrix-org/matrix-spec-proposals/pull/4426
    pub fn enable_automatic_call_status(&self, enabled: bool) {
        let mut automatic_call_status = self.inner.automatic_call_status.lock().unwrap();
        match (enabled, automatic_call_status.is_some()) {
            (true, false) => {
                *automatic_call_status = Some(AutomaticCallStatus::new(self));
            }
            (false, true) => *automatic_call_status = None,
            _ => {}
        }
    }
}

impl AutomaticCallStatus {
    fn new(client: &Client) -> Self {
        // Start empty: `m.call` is shared across the user's devices, so we
        // deliberately don't reconcile from current room state on start-up
        // to avoid stomping on a status set by another device. The
        // trade-off is that a crash/kill while on a call leaves `m.call`
        // set until the user clears it manually.
        let rooms: ActiveCallRooms = Arc::new(Mutex::new(HashSet::new()));
        let handle = client.add_event_handler(
            async move |event: OriginalSyncStateEvent<CallMemberEventContent>,
                        room: Room,
                        client: Client| {
                on_event(&rooms, event, room, client);
            },
        );
        let weak_client = WeakClient::from_client(client);
        Self { handle, client: weak_client }
    }
}

impl Drop for AutomaticCallStatus {
    fn drop(&mut self) {
        if let Some(client) = self.client.get() {
            client.remove_event_handler(self.handle.clone());
        }
    }
}

fn on_event(
    rooms: &ActiveCallRooms,
    event: OriginalSyncStateEvent<CallMemberEventContent>,
    room: Room,
    client: Client,
) {
    let Some(own_user_id) = client.user_id() else { return };
    let Some(own_device_id) = client.device_id() else { return };

    // Ignore events for other users' memberships.
    if event.state_key.user_id() != own_user_id {
        return;
    }

    // Update the aggregate for this room, then return early if the "in any
    // call" boolean didn't flip.
    let is_device_in_room_call = room.is_device_in_active_room_call(own_user_id, own_device_id);
    let room_id = room.room_id().to_owned();
    let (was_in_call, now_in_call) = {
        let mut rooms = rooms.lock().unwrap();
        let was_in_call = !rooms.is_empty();
        if is_device_in_room_call {
            rooms.insert(room_id);
        } else {
            rooms.remove(&room_id);
        }
        (was_in_call, !rooms.is_empty())
    };
    // Return early if no change.
    if was_in_call == now_in_call {
        return;
    }
    let active_call_rooms = rooms.clone();
    spawn(async move {
        let in_call = !active_call_rooms.lock().unwrap().is_empty();
        let result = if in_call {
            let joined_ts = SecondsSinceUnixEpoch::from_system_time(SystemTime::now());
            client.account().set_call(joined_ts).await
        } else {
            client.account().clear_call().await
        };
        if let Err(error) = result {
            warn!(?error, in_call, "m.call auto-sync request failed");
        }
    });
}
