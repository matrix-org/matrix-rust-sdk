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

use ruma::OwnedRoomId;

/// Represents a timeline update of a room. It hides the details of
/// [`RoomEventCacheUpdate`] by being more generic.
///
/// This is used by [`EventCache::subscribe_to_room_generic_updates`]. Please
/// read it to learn more about the motivation behind this type.
#[derive(Clone, Debug)]
pub struct RoomEventCacheGenericUpdate {
    /// The room ID owning the timeline.
    pub room_id: OwnedRoomId,
}
