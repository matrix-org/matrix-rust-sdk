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
pub mod changes;
#[cfg(feature = "e2e-encryption")]
pub mod e2ee;
pub mod ephemeral_events;
pub mod notification;
pub mod profiles;
pub mod room;
pub mod state_events;
pub mod timeline;
#[cfg(feature = "e2e-encryption")]
pub mod verification;

use std::collections::BTreeMap;

use ruma::OwnedRoomId;

use crate::{RoomInfoNotableUpdateReasons, StateChanges};

type RoomInfoNotableUpdates = BTreeMap<OwnedRoomId, RoomInfoNotableUpdateReasons>;

#[cfg_attr(test, derive(Clone))]
#[derive(Default)]
pub(crate) struct Context {
    pub(super) state_changes: StateChanges,
    pub(super) room_info_notable_updates: RoomInfoNotableUpdates,
}

impl Context {
    pub fn new(state_changes: StateChanges) -> Self {
        Self { state_changes, room_info_notable_updates: Default::default() }
    }
}
