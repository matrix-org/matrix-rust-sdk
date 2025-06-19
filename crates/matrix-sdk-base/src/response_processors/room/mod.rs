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

use ruma::RoomId;
use tokio::sync::broadcast::Sender;

use crate::{RequestedRequiredStates, RoomInfoNotableUpdate, store::ambiguity_map::AmbiguityCache};

pub mod display_name;
pub mod msc4186;
pub mod sync_v2;

/// A classical set of data used by some processors in this module.
pub struct RoomCreationData<'a> {
    room_id: &'a RoomId,
    room_info_notable_update_sender: Sender<RoomInfoNotableUpdate>,
    requested_required_states: &'a RequestedRequiredStates,
    ambiguity_cache: &'a mut AmbiguityCache,
}

impl<'a> RoomCreationData<'a> {
    pub fn new(
        room_id: &'a RoomId,
        room_info_notable_update_sender: Sender<RoomInfoNotableUpdate>,
        requested_required_states: &'a RequestedRequiredStates,
        ambiguity_cache: &'a mut AmbiguityCache,
    ) -> Self {
        Self {
            room_id,
            room_info_notable_update_sender,
            requested_required_states,
            ambiguity_cache,
        }
    }
}
