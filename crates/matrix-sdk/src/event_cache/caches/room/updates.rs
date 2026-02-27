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

use matrix_sdk_base::{
    event_cache::{Event, Gap},
    linked_chunk::{self, OwnedLinkedChunkId},
};
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

/// An update being triggered when events change in the persisted event cache
/// for any room.
#[derive(Clone, Debug)]
pub struct RoomEventCacheLinkedChunkUpdate {
    /// The linked chunk affected by the update.
    pub linked_chunk_id: OwnedLinkedChunkId,

    /// A vector of all the linked chunk updates that happened during this event
    /// cache update.
    pub updates: Vec<linked_chunk::Update<Event, Gap>>,
}

impl RoomEventCacheLinkedChunkUpdate {
    /// Return all the new events propagated by this update, in topological
    /// order.
    pub fn events(self) -> impl DoubleEndedIterator<Item = Event> {
        use itertools::Either;
        self.updates.into_iter().flat_map(|update| match update {
            linked_chunk::Update::PushItems { items, .. } => {
                Either::Left(Either::Left(items.into_iter()))
            }
            linked_chunk::Update::ReplaceItem { item, .. } => {
                Either::Left(Either::Right(std::iter::once(item)))
            }
            linked_chunk::Update::RemoveItem { .. }
            | linked_chunk::Update::DetachLastItems { .. }
            | linked_chunk::Update::StartReattachItems
            | linked_chunk::Update::EndReattachItems
            | linked_chunk::Update::NewItemsChunk { .. }
            | linked_chunk::Update::NewGapChunk { .. }
            | linked_chunk::Update::RemoveChunk(..)
            | linked_chunk::Update::Clear => {
                // All these updates don't contain any new event.
                Either::Right(std::iter::empty())
            }
        })
    }
}
