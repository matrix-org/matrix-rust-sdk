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

pub mod account_data;
#[cfg(feature = "e2e-encryption")]
pub mod e2ee;
#[cfg(feature = "e2e-encryption")]
pub mod latest_event;
#[cfg(feature = "e2e-encryption")]
pub mod verification;

use std::collections::BTreeMap;

#[cfg(feature = "e2e-encryption")]
mod with_e2ee {
    pub use super::{e2ee::e2ee, latest_event::decrypt_latest_events, verification::verification};
}
use ruma::OwnedRoomId;
#[cfg(feature = "e2e-encryption")]
pub use with_e2ee::*;

use crate::{RoomInfoNotableUpdateReasons, StateChanges};

type RoomInfoNotableUpdates = BTreeMap<OwnedRoomId, RoomInfoNotableUpdateReasons>;

pub(crate) struct Context {
    pub(super) state_changes: StateChanges,
    pub(super) room_info_notable_updates: RoomInfoNotableUpdates,
}

impl Context {
    pub fn new(
        state_changes: StateChanges,
        room_info_notable_updates: RoomInfoNotableUpdates,
    ) -> Self {
        Self { state_changes, room_info_notable_updates }
    }

    pub fn into_parts(self) -> (StateChanges, RoomInfoNotableUpdates) {
        let Self { state_changes, room_info_notable_updates } = self;

        (state_changes, room_info_notable_updates)
    }
}
