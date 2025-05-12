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

use eyeball::{AsyncLock, SharedObservable, Subscriber};
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};

/// The latest event of a room or a thread.
///
/// Use [`LatestEvent::subscribe`] to get a stream of updates.
#[derive(Debug)]
pub(super) struct LatestEvent {
    /// The room owning this latest event.
    _room_id: OwnedRoomId,
    /// The thread (if any) owning this latest event.
    _thread_id: Option<OwnedEventId>,
    /// The latest event value.
    value: SharedObservable<LatestEventValue, AsyncLock>,
}

impl LatestEvent {
    pub(super) fn new(room_id: &RoomId, thread_id: Option<&EventId>) -> Option<Self> {
        Some(Self {
            _room_id: room_id.to_owned(),
            _thread_id: thread_id.map(ToOwned::to_owned),
            value: SharedObservable::new_async(LatestEventValue::None),
        })
    }

    /// Return a [`Subscriber`] to new values.
    pub async fn subscribe(&self) -> Subscriber<LatestEventValue, AsyncLock> {
        self.value.subscribe().await
    }
}

/// A latest event value!
#[derive(Debug, Clone)]
pub enum LatestEventValue {
    /// No value has been computed yet, or no candidate value was found.
    None,
}
