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
    deserialized_responses::AmbiguityChange,
    event_cache::{Event, Gap},
    linked_chunk::{self, OwnedLinkedChunkId},
};
use ruma::{
    OwnedEventId, OwnedMxcUri, OwnedRoomId, OwnedUserId, events::AnySyncEphemeralRoomEvent,
    serde::Raw,
};
use tokio::sync::{
    broadcast::{Receiver, Sender},
    mpsc::{UnboundedReceiver, UnboundedSender},
};

use super::super::{super::EventsOrigin, TimelineVectorDiffs};

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

    /// The set of gaps in the loaded part of the room's timeline has changed.
    ///
    /// This is a full snapshot, not a diff: it replaces any previously
    /// received set. Consumers (i.e. the timeline) are expected to reconcile
    /// their gap markers against it: insert markers for new gaps, remove
    /// markers for gaps that are no longer present (because they've been
    /// resolved, or unloaded from memory).
    UpdateTimelineGaps {
        /// All the gaps currently present in the in-memory linked chunk, in
        /// timeline order (oldest first).
        gaps: Vec<TimelineGap>,
    },

    /// The room has received new ephemeral events.
    AddEphemeralEvents {
        /// XXX: this is temporary, until read receipts are handled in the event
        /// cache
        events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
    },
}

/// A gap in the loaded part of a room's timeline, as exposed to timeline
/// consumers.
///
/// A gap materializes a range of events we know nothing about: it can be
/// resolved (i.e. filled with events) with
/// [`RoomEventCache::resolve_gap`][super::RoomEventCache::resolve_gap].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimelineGap {
    /// The previous-batch token identifying this gap; to be used as the `end`
    /// parameter of the back-pagination request that resolves it.
    pub prev_token: String,

    /// The ID of the first event following this gap in the linked chunk,
    /// used to anchor the gap in the timeline.
    ///
    /// `None` when no known event follows the gap: the gap then sits at the
    /// newest end of whatever the timeline shows. The room event cache never
    /// reports such trailing gaps (there is nothing to anchor them to, and
    /// nothing to separate them from); message-type filtered views do (see
    /// [`MessageTypesEventCache`]), since "no media after this gap" is
    /// exactly what a media grid needs to show a spinner for.
    ///
    /// [`MessageTypesEventCache`]: crate::event_cache::MessageTypesEventCache
    pub following_event_id: Option<OwnedEventId>,
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

    /// Where the events triggering this update came from: a sync response,
    /// a back-pagination, or the cache itself (initial load, redecryption,
    /// cross-process reload).
    pub origin: EventsOrigin,
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

    /// Events replaced in place in the store, by event ID, without a linked
    /// chunk update: a redecryption of events that aren't loaded in memory
    /// (those in memory are replaced through `updates`). Observers keyed on
    /// the linked chunk (a message-type filtered view) would otherwise keep
    /// showing the undecryptable event.
    pub replaced_events: Vec<Event>,
}

impl RoomEventCacheLinkedChunkUpdate {
    /// Return all the new events propagated by this update, in topological
    /// order.
    pub fn events(self) -> impl DoubleEndedIterator<Item = Event> {
        use itertools::Either;
        let Self { updates, replaced_events, .. } = self;
        updates
            .into_iter()
            .flat_map(|update| match update {
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
            .chain(replaced_events)
    }
}

/// A lossless multi-consumer channel of
/// [`RoomEventCacheLinkedChunkUpdate`]s: like a broadcast channel, but backed
/// by an unbounded queue per subscriber, so a slow consumer lags in delivery
/// instead of losing updates.
///
/// The linked chunk updates feed the search index, thread subscriptions and
/// re-decryption; with a broadcast channel, all of those silently missed
/// updates whenever they fell more than the channel capacity behind (e.g.
/// during a busy catch-up), leaving permanent holes in the search index.
#[derive(Clone, Debug)]
pub(crate) struct LinkedChunkUpdateFanout {
    subscribers:
        std::sync::Arc<std::sync::Mutex<Vec<UnboundedSender<RoomEventCacheLinkedChunkUpdate>>>>,
}

impl LinkedChunkUpdateFanout {
    /// Create a new, subscriber-less fanout.
    pub fn new() -> Self {
        Self { subscribers: Default::default() }
    }

    /// Deliver an update to every live subscriber.
    pub fn send(&self, update: RoomEventCacheLinkedChunkUpdate) {
        self.subscribers.lock().unwrap().retain(|tx| tx.send(update.clone()).is_ok());
    }

    /// Subscribe to all updates sent from now on.
    pub fn subscribe(&self) -> UnboundedReceiver<RoomEventCacheLinkedChunkUpdate> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.subscribers.lock().unwrap().push(tx);
        rx
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

#[cfg(test)]
mod tests {
    use matrix_sdk_common::linked_chunk::{ChunkIdentifier, OwnedLinkedChunkId, Update};
    use ruma::room_id;

    use super::{LinkedChunkUpdateFanout, RoomEventCacheLinkedChunkUpdate};

    fn dummy_update() -> RoomEventCacheLinkedChunkUpdate {
        RoomEventCacheLinkedChunkUpdate {
            linked_chunk_id: OwnedLinkedChunkId::Room(room_id!("!room:example.org").to_owned()),
            updates: vec![Update::NewItemsChunk {
                previous: None,
                new: ChunkIdentifier::new(0),
                next: None,
            }],
            replaced_events: Vec::new(),
        }
    }

    #[test]
    fn test_fanout_delivers_everything_to_slow_subscribers() {
        let fanout = LinkedChunkUpdateFanout::new();

        // Updates sent before subscribing are not delivered.
        fanout.send(dummy_update());

        let mut rx = fanout.subscribe();

        // A subscriber that doesn't consume anything for a while still gets
        // every update, in order: this is the property the broadcast channel
        // lacked (it dropped the oldest updates beyond its capacity).
        const NUM_UPDATES: usize = 4096;
        for _ in 0..NUM_UPDATES {
            fanout.send(dummy_update());
        }

        let mut received = 0;
        while let Ok(_up) = rx.try_recv() {
            received += 1;
        }
        assert_eq!(received, NUM_UPDATES);

        // A dropped subscriber is pruned on the next send rather than
        // accumulating queued updates forever.
        drop(rx);
        fanout.send(dummy_update());
        assert_eq!(fanout.subscribers.lock().unwrap().len(), 0);
    }
}
