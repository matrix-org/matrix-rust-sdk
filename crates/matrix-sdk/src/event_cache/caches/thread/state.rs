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

use std::collections::BTreeSet;

use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    event_cache::{Event, Gap, store::EventCacheStoreLock},
    linked_chunk::{OwnedLinkedChunkId, Position, Update},
};
use matrix_sdk_common::executor::spawn;
use ruma::{OwnedEventId, OwnedRoomId};
use tokio::sync::broadcast::Sender;
use tracing::instrument;

use super::super::{
    super::{EventsOrigin, Result, deduplicator::DeduplicationOutcome},
    TimelineVectorDiffs,
    event_linked_chunk::EventLinkedChunk,
    lock,
    room::RoomEventCacheLinkedChunkUpdate,
};

pub struct ThreadEventCacheState {
    /// The room owning this thread.
    #[allow(dead_code)] // for the persistent storage
    room_id: OwnedRoomId,

    /// The ID of the thread root event, which is the first event in the thread
    /// (and eventually the first in the linked chunk).
    thread_id: OwnedEventId,

    /// Reference to the underlying backing store.
    store: EventCacheStoreLock,

    /// The linked chunk for this thread.
    thread_linked_chunk: EventLinkedChunk,

    /// A sender for live events updates in this thread.
    pub sender: Sender<TimelineVectorDiffs>,

    /// A sender for the globally observable linked chunk updates that happened
    /// during a sync or a back-pagination.
    ///
    /// See also [`super::super::EventCacheInner::linked_chunk_update_sender`].
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,

    /// Have we ever waited for a previous-batch-token to come from sync, in
    /// the context of pagination? We do this at most once per room/thread (?),
    /// the first time we try to run backward pagination. We reset
    /// that upon clearing the timeline events.
    waited_for_initial_prev_token: bool,
}

impl lock::Store for ThreadEventCacheState {
    fn store(&self) -> &EventCacheStoreLock {
        &self.store
    }
}

/// State for a single thread's event cache.
///
/// This contains all the inner mutable states that ought to be updated at
/// the same time.
pub type LockedThreadEventCacheState = lock::StateLock<ThreadEventCacheState>;

impl LockedThreadEventCacheState {
    /// Create a new state, or reload it from storage if it's been enabled.
    ///
    /// Not all events are going to be loaded. Only a portion of them. The
    /// [`EventLinkedChunk`] relies on a [`LinkedChunk`] to store all
    /// events. Only the last chunk will be loaded. It means the
    /// events are loaded from the most recent to the oldest. To
    /// load more events, see [`ThreadPagination`].
    ///
    /// [`LinkedChunk`]: matrix_sdk_common::linked_chunk::LinkedChunk
    /// [`ThreadPagination`]: super::pagination::ThreadPagination
    pub fn new(
        room_id: OwnedRoomId,
        thread_id: OwnedEventId,
        store: EventCacheStoreLock,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
    ) -> Self {
        Self::new_inner(ThreadEventCacheState {
            room_id,
            thread_id,
            store,
            thread_linked_chunk: EventLinkedChunk::new(),
            sender: Sender::new(32),
            linked_chunk_update_sender,
            waited_for_initial_prev_token: false,
        })
    }
}

/// The read-lock guard around [`ThreadEventCacheState`].
///
/// See [`ThreadEventCacheStateLock::read`] to acquire it.
pub type ThreadEventCacheStateLockReadGuard<'a> =
    lock::StateLockReadGuard<'a, ThreadEventCacheState>;

/// The write-lock guard around [`ThreadEventCacheState`].
///
/// See [`ThreadEventCacheStateLock::write`] to acquire it.
pub type ThreadEventCacheStateLockWriteGuard<'a> =
    lock::StateLockWriteGuard<'a, ThreadEventCacheState>;

impl<'a> lock::Reload for ThreadEventCacheStateLockWriteGuard<'a> {
    /// Force to shrink the room, whenever there is subscribers or not.
    async fn reload(&mut self) -> Result<()> {
        self.state.thread_linked_chunk.reset();

        let diffs = self.state.thread_linked_chunk.updates_as_vector_diffs();

        if !diffs.is_empty() {
            let _ =
                self.state.sender.send(TimelineVectorDiffs { diffs, origin: EventsOrigin::Cache });
        }

        Ok(())
    }
}

impl<'a> ThreadEventCacheStateLockReadGuard<'a> {
    /// Return a read-only reference to the underlying thread linked chunk.
    pub fn thread_linked_chunk(&self) -> &EventLinkedChunk {
        &self.state.thread_linked_chunk
    }

    /// Get the `waited_for_initial_prev_token` value.
    pub fn waited_for_initial_prev_token(&self) -> bool {
        self.state.waited_for_initial_prev_token
    }
}

impl<'a> ThreadEventCacheStateLockWriteGuard<'a> {
    /// Return a read-only reference to the underlying thread linked chunk.
    pub fn thread_linked_chunk(&self) -> &EventLinkedChunk {
        &self.state.thread_linked_chunk
    }

    /// Return a mutable reference to the underlying thread linked chunk.
    pub fn thread_linked_chunk_mut(&mut self) -> &mut EventLinkedChunk {
        &mut self.state.thread_linked_chunk
    }

    /// Get the `waited_for_initial_prev_token` value.
    pub fn waited_for_initial_prev_token_mut(&mut self) -> &mut bool {
        &mut self.state.waited_for_initial_prev_token
    }

    pub async fn handle_sync(&mut self, events: Vec<Event>) -> Result<Vec<VectorDiff<Event>>> {
        let deduplication = self.filter_duplicate_events(events);

        if deduplication.non_empty_all_duplicates {
            // If all events are duplicates, we don't need to do anything; ignore
            // the new events.
            return Ok(Vec::new());
        }

        // Remove the duplicated events from the thread chunk.
        self.remove_events(deduplication.in_memory_duplicated_event_ids).await?;
        assert!(
            deduplication.in_store_duplicated_event_ids.is_empty(),
            "persistent storage for threads is not implemented yet"
        );

        let events = deduplication.all_events;

        self.state.thread_linked_chunk.push_live_events(None, &events);

        self.propagate_changes().await?;

        let timeline_event_diffs = self.state.thread_linked_chunk.updates_as_vector_diffs();

        Ok(timeline_event_diffs)
    }

    /// Save events into the database, without notifying observers.
    pub async fn save_events(&mut self, events: impl IntoIterator<Item = Event>) -> Result<()> {
        let store = self.store.clone();
        let room_id = self.state.room_id.clone();
        let events = events.into_iter().collect::<Vec<_>>();

        // Spawn a task so the save is uninterrupted by task cancellation.
        spawn(async move {
            for event in events {
                store.save_event(&room_id, event).await?;
            }

            Result::Ok(())
        })
        .await
        .expect("joining failed")?;

        Ok(())
    }

    /// Reset this data structure as if it were brand new.
    ///
    /// Return a single diff update that is a clear of all events; as a
    /// result, the caller may override any pending diff updates
    /// with the result of this function.
    pub async fn reset(&mut self) -> Result<Vec<VectorDiff<Event>>> {
        self.reset_internal().await?;

        let diff_updates = self.state.thread_linked_chunk.updates_as_vector_diffs();

        // Ensure the contract defined in the doc comment is true:
        debug_assert_eq!(diff_updates.len(), 1);
        debug_assert!(matches!(diff_updates[0], VectorDiff::Clear));

        Ok(diff_updates)
    }

    async fn reset_internal(&mut self) -> Result<()> {
        self.state.thread_linked_chunk.reset();

        self.propagate_changes().await?;

        // Reset the pagination state too: pretend we never waited for the initial
        // prev-batch token, and indicate that we're not at the start of the
        // timeline, since we don't know about that anymore.
        self.state.waited_for_initial_prev_token = false;

        Ok(())
    }

    /// Remove events by their position, in `EventLinkedChunk`.
    ///
    /// This method is purposely isolated because it must ensure that
    /// positions are sorted appropriately or it can be disastrous.
    ///
    /// TODO: support store.
    #[instrument(skip_all)]
    pub async fn remove_events(
        &mut self,
        in_memory_events: Vec<(OwnedEventId, Position)>,
    ) -> Result<()> {
        // In-memory events.
        if in_memory_events.is_empty() {
            // Nothing else to do, return early.
            return Ok(());
        }

        // `remove_events_by_position` is responsible of sorting positions.
        self.state
            .thread_linked_chunk
            .remove_events_by_position(
                in_memory_events.into_iter().map(|(_event_id, position)| position).collect(),
            )
            .expect("failed to remove an event");

        self.propagate_changes().await
    }

    pub async fn propagate_changes(&mut self) -> Result<()> {
        let updates = self.state.thread_linked_chunk.store_updates().take();

        self.send_updates_to_store(updates).await
    }

    #[allow(clippy::unused_async)] // TODO: remove once persistent storage is implemented
    async fn send_updates_to_store(&mut self, updates: Vec<Update<Event, Gap>>) -> Result<()> {
        // TODO: call the `persistence::send_updates_to_store` function
        let linked_chunk_id =
            OwnedLinkedChunkId::Thread(self.state.room_id.clone(), self.state.thread_id.clone());

        let _ = self
            .state
            .linked_chunk_update_sender
            .send(RoomEventCacheLinkedChunkUpdate { linked_chunk_id, updates });

        Ok(())
    }

    /// Find duplicates in a thread, until there's persistent storage for
    /// those.
    ///
    /// TODO: when persistent storage is implemented for thread, only use
    /// the regular `filter_duplicate_events` method.
    pub fn filter_duplicate_events(&self, mut new_events: Vec<Event>) -> DeduplicationOutcome {
        let mut new_event_ids = BTreeSet::new();

        new_events.retain(|event| {
            // Only keep events with IDs, and those for which `insert` returns `true`
            // (meaning they were not in the set).
            event.event_id().is_some_and(|event_id| new_event_ids.insert(event_id))
        });

        let in_memory_duplicated_event_ids: Vec<_> = self
            .state
            .thread_linked_chunk
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
}
