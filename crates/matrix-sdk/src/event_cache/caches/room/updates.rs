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

use std::collections::BTreeMap;

use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, ThreadSummary},
    event_cache::{Event, Gap},
    linked_chunk::{self, OwnedLinkedChunkId},
};
use ruma::{
    OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, events::receipt::ReceiptEventContent,
};
use tokio::sync::broadcast::{Receiver, Sender};

use super::super::TimelineVectorDiffs;

/// An update related to events happened in a room.
#[derive(Debug, Clone)]
pub enum RoomEventCacheUpdate {
    /// The fully read marker has moved to a different event.
    MoveReadMarkerTo {
        /// Event at which the read marker is now pointing.
        event_id: OwnedEventId,
    },

    /// The members have changed.
    UpdateMembers {
        /// Collection of ambiguity changes that room member events trigger.
        ///
        /// This is a map of event ID of the `m.room.member` event to the
        /// details of the ambiguity change.
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,

        /// Collection of avatar changes that room member events trigger.
        avatar_changes: Option<BTreeMap<OwnedUserId, Option<OwnedMxcUri>>>,
    },

    /// The room has received updates for the timeline as _diffs_.
    UpdateTimelineEvents(TimelineVectorDiffs),

    /// An event, which is a thread root, has received an update for its thread
    /// summary.
    UpdateThreadSummary {
        /// The event ID of the thread root.
        ///
        /// Also called _thread ID_.
        thread_root: OwnedEventId,

        /// The updated thread summary.
        thread_summary: ThreadSummary,
    },

    /// The room has received a new read receipt event.
    AddReadReceiptEvent {
        /// The event containing the receipts.
        event: ReceiptEventContent,
    },
}

/// Represents a timeline update of a room. It hides the details of
/// [`RoomEventCacheUpdate`] by being more generic.
///
/// This is used by [`EventCache::subscribe_to_room_generic_updates`][0]. Please
/// read it to learn more about the motivation behind this type.
///
/// [0]: super::super::super::EventCache::subscribe_to_room_generic_updates
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

/// Sends [`RoomEventCacheLinkedChunkUpdate`]s to their observers.
///
/// The search index gets its own unbounded channel: a broadcast receiver that
/// lags behind drops updates, whose events would then never get indexed.
#[derive(Clone, Debug)]
pub struct LinkedChunkUpdateSender {
    sender: Sender<RoomEventCacheLinkedChunkUpdate>,

    #[cfg(feature = "experimental-search")]
    search_index_sender: tokio::sync::mpsc::UnboundedSender<(OwnedRoomId, Vec<Event>)>,
}

impl LinkedChunkUpdateSender {
    /// Create a new [`LinkedChunkUpdateSender`], sending room events to be
    /// indexed to `search_index_sender`.
    pub fn new(
        #[cfg(feature = "experimental-search")]
        search_index_sender: tokio::sync::mpsc::UnboundedSender<(OwnedRoomId, Vec<Event>)>,
    ) -> Self {
        Self {
            sender: Sender::new(128),
            #[cfg(feature = "experimental-search")]
            search_index_sender,
        }
    }

    /// Send an update to all the observers.
    pub fn send(&self, update: RoomEventCacheLinkedChunkUpdate) {
        #[cfg(feature = "experimental-search")]
        if let OwnedLinkedChunkId::Room(room_id) = &update.linked_chunk_id {
            self.send_to_search_index(room_id.clone(), update.clone().events().collect());
        }

        let _ = self.sender.send(update);
    }

    /// Send events of a room to the search index only.
    #[cfg(feature = "experimental-search")]
    pub fn send_to_search_index(&self, room_id: OwnedRoomId, events: Vec<Event>) {
        if !events.is_empty() {
            let _ = self.search_index_sender.send((room_id, events));
        }
    }

    /// Subscribe to the updates.
    pub fn subscribe(&self) -> Receiver<RoomEventCacheLinkedChunkUpdate> {
        self.sender.subscribe()
    }
}

/// A small type to send updates in all channels.
#[derive(Clone)]
pub struct RoomEventCacheUpdateSender {
    room_sender: Sender<RoomEventCacheUpdate>,
    generic_sender: Sender<RoomEventCacheGenericUpdate>,
}

impl RoomEventCacheUpdateSender {
    /// Create a new [`RoomEventCacheUpdateSender`].
    pub fn new(generic_sender: Sender<RoomEventCacheGenericUpdate>) -> Self {
        Self { room_sender: Sender::new(32), generic_sender }
    }

    /// Send a [`RoomEventCacheUpdate`] and an optional
    /// [`RoomEventCacheGenericUpdate`].
    pub fn send(
        &self,
        room_update: RoomEventCacheUpdate,
        generic_update: Option<RoomEventCacheGenericUpdate>,
    ) {
        let _ = self.room_sender.send(room_update);

        if let Some(generic_update) = generic_update {
            let _ = self.generic_sender.send(generic_update);
        }
    }

    /// Get the generic update sender.
    pub(in super::super) fn generic_update_sender(&self) -> &Sender<RoomEventCacheGenericUpdate> {
        &self.generic_sender
    }

    /// Create a new [`Receiver`] of [`RoomEventCacheUpdate`].
    pub(super) fn new_room_receiver(&self) -> Receiver<RoomEventCacheUpdate> {
        self.room_sender.subscribe()
    }
}
