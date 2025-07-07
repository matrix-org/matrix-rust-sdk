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

//! Threads-related data structures.

use std::collections::BTreeSet;

use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    event_cache::{Event, Gap},
    linked_chunk::{ChunkContent, Position},
};
use ruma::OwnedEventId;
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::trace;

use crate::event_cache::{
    deduplicator::DeduplicationOutcome,
    room::{events::EventLinkedChunk, LoadMoreEventsBackwardsOutcome},
    BackPaginationOutcome, EventsOrigin,
};

/// An update coming from a thread event cache.
#[derive(Clone, Debug)]
pub struct ThreadEventCacheUpdate {
    /// New vector diff for the thread timeline.
    pub diffs: Vec<VectorDiff<Event>>,
    /// The origin that triggered this update.
    pub origin: EventsOrigin,
}

/// All the information related to a single thread.
pub(crate) struct ThreadEventCache {
    /// The ID of the thread root event, which is the first event in the thread
    /// (and eventually the first in the linked chunk).
    thread_root: OwnedEventId,

    /// The linked chunk for this thread.
    chunk: EventLinkedChunk,

    /// A sender for live events updates in this thread.
    sender: Sender<ThreadEventCacheUpdate>,
}

impl ThreadEventCache {
    /// Create a new empty thread event cache.
    pub fn new(thread_root: OwnedEventId) -> Self {
        Self { chunk: EventLinkedChunk::new(), sender: Sender::new(32), thread_root }
    }

    /// Subscribe to live events from this thread.
    pub fn subscribe(&self) -> (Vec<Event>, Receiver<ThreadEventCacheUpdate>) {
        let events = self.chunk.events().map(|(_position, item)| item.clone()).collect();

        let recv = self.sender.subscribe();

        (events, recv)
    }

    /// Clear a thread, after a gappy sync for instance.
    pub fn clear(&mut self) {
        self.chunk.reset();

        let diffs = self.chunk.updates_as_vector_diffs();
        if !diffs.is_empty() {
            let _ = self.sender.send(ThreadEventCacheUpdate { diffs, origin: EventsOrigin::Cache });
        }
    }

    /// Push some live events to this thread, and propagate the updates to
    /// the listeners.
    pub fn add_live_events(&mut self, events: Vec<Event>) {
        if events.is_empty() {
            return;
        }

        let deduplication = self.filter_duplicate_events(events);

        if deduplication.non_empty_all_duplicates {
            // If all events are duplicates, we don't need to do anything; ignore
            // the new events.
            return;
        }

        // Remove the duplicated events from the thread chunk.
        self.remove_in_memory_duplicated_events(deduplication.in_memory_duplicated_event_ids);
        assert!(
            deduplication.in_store_duplicated_event_ids.is_empty(),
            "persistent storage for threads is not implemented yet"
        );

        let events = deduplication.all_events;

        self.chunk.push_live_events(None, &events);

        let diffs = self.chunk.updates_as_vector_diffs();
        if !diffs.is_empty() {
            let _ = self.sender.send(ThreadEventCacheUpdate { diffs, origin: EventsOrigin::Sync });
        }
    }

    /// Simplified version of
    /// [`RoomEventCacheState::load_more_events_backwards`], which
    /// returns the outcome of the pagination without actually loading from
    /// disk.
    pub fn load_more_events_backwards(&self) -> LoadMoreEventsBackwardsOutcome {
        // If any in-memory chunk is a gap, don't load more events, and let the caller
        // resolve the gap.
        if let Some(prev_token) = self.chunk.rgap().map(|gap| gap.prev_token) {
            trace!(%prev_token, "thread chunk has at least a gap");
            return LoadMoreEventsBackwardsOutcome::Gap { prev_token: Some(prev_token) };
        }

        // If we don't have a gap, then the first event should be the the thread's root;
        // otherwise, we'll restart a pagination from the end.
        if let Some((_pos, event)) = self.chunk.events().next() {
            let first_event_id =
                event.event_id().expect("a linked chunk only stores events with IDs");

            if first_event_id == self.thread_root {
                trace!("thread chunk is fully loaded and non-empty: reached_start=true");
                return LoadMoreEventsBackwardsOutcome::StartOfTimeline;
            }
        }

        // Otherwise, we don't have a gap nor events. We don't have anything. Poor us.
        // Well, is ok: start a pagination from the end.
        LoadMoreEventsBackwardsOutcome::Gap { prev_token: None }
    }

    /// Find duplicates in a thread, until there's persistent storage for
    /// those.
    ///
    /// TODO: when persistent storage is implemented for thread, only use
    /// the regular `filter_duplicate_events` method.
    fn filter_duplicate_events(&self, mut new_events: Vec<Event>) -> DeduplicationOutcome {
        let mut new_event_ids = BTreeSet::new();

        new_events.retain(|event| {
            // Only keep events with IDs, and those for which `insert` returns `true`
            // (meaning they were not in the set).
            event.event_id().is_some_and(|event_id| new_event_ids.insert(event_id))
        });

        let in_memory_duplicated_event_ids: Vec<_> = self
            .chunk
            .events()
            .filter_map(|(position, event)| {
                let event_id = event.event_id()?;
                new_event_ids.contains(&event_id).then_some((event_id, position))
            })
            .collect();

        // Right now, there's no persistent storage for threads.
        let in_store_duplicated_event_ids = Vec::new();

        let at_least_one_event = !new_events.is_empty();
        let all_duplicates = (in_memory_duplicated_event_ids.len()
            + in_store_duplicated_event_ids.len())
            == new_events.len();
        let non_empty_all_duplicates = at_least_one_event && all_duplicates;

        DeduplicationOutcome {
            all_events: new_events,
            in_memory_duplicated_event_ids,
            in_store_duplicated_event_ids,
            non_empty_all_duplicates,
        }
    }

    /// Remove in-memory duplicated events from the thread chunk, that have
    /// been found with [`Self::filter_duplicate_events`].
    ///
    /// Precondition: the `in_memory_duplicated_event_ids` must be the
    /// result of the above function, otherwise this can panic.
    fn remove_in_memory_duplicated_events(
        &mut self,
        in_memory_duplicated_event_ids: Vec<(OwnedEventId, Position)>,
    ) {
        // Remove the duplicated events from the thread chunk.
        self.chunk
            .remove_events_by_position(
                in_memory_duplicated_event_ids
                    .iter()
                    .map(|(_event_id, position)| *position)
                    .collect(),
            )
            .expect("we collected the position of the events to remove just before");
    }

    /// Finish a network pagination started with the gap retrieved from
    /// [`Self::load_more_events_backwards`].
    ///
    /// Returns `None` if the gap couldn't be found anymore (meaning the
    /// thread has been reset while the pagination was ongoing).
    pub fn finish_network_pagination(
        &mut self,
        prev_token: Option<String>,
        new_token: Option<String>,
        events: Vec<Event>,
    ) -> Option<BackPaginationOutcome> {
        // TODO(bnjbvr): consider deduplicating this code (~same for room) at some
        // point.
        let prev_gap_id = if let Some(token) = prev_token {
            // If the gap id is missing, it means that the gap disappeared during
            // pagination; in this case, early return to the caller.
            let gap_id = self.chunk.chunk_identifier(|chunk| {
                    matches!(chunk.content(), ChunkContent::Gap(Gap { ref prev_token }) if *prev_token == token)
                })?;

            Some(gap_id)
        } else {
            None
        };

        // This is a backwards pagination, so the events were returned in the reverse
        // topological order.
        let topo_ordered_events = events.iter().cloned().rev().collect::<Vec<_>>();
        let new_gap = new_token.map(|token| Gap { prev_token: token });

        let deduplication = self.filter_duplicate_events(topo_ordered_events);

        assert!(
            deduplication.in_store_duplicated_event_ids.is_empty(),
            "persistent storage for threads is not implemented yet"
        );
        self.remove_in_memory_duplicated_events(deduplication.in_memory_duplicated_event_ids);

        let (events, new_gap) = if deduplication.non_empty_all_duplicates {
            // If all events are duplicates, we don't need to do anything; ignore
            // the new events and the new gap.
            (Vec::new(), None)
        } else {
            // Keep events and the gap.
            (deduplication.all_events, new_gap)
        };

        // Add the paginated events to the thread chunk.
        let reached_start = self.chunk.finish_back_pagination(prev_gap_id, new_gap, &events);

        // Notify observers about the updates.
        let updates = self.chunk.updates_as_vector_diffs();
        if !updates.is_empty() {
            // Send the updates to the listeners.
            let _ = self
                .sender
                .send(ThreadEventCacheUpdate { diffs: updates, origin: EventsOrigin::Pagination });
        }

        Some(BackPaginationOutcome { reached_start, events })
    }

    /// Returns the latest event ID in this thread, if any.
    pub fn latest_event_id(&self) -> Option<OwnedEventId> {
        self.chunk.revents().next().and_then(|(_position, event)| event.event_id())
    }
}
