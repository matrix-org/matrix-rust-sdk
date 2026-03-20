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

use std::sync::Arc;

use eyeball::SharedObservable;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    event_cache::{Event, Gap},
    linked_chunk::ChunkContent,
};
use ruma::api::Direction;
use tracing::trace;

use super::{
    super::super::{
        EventCacheError, EventsOrigin, Result, TimelineVectorDiffs,
        caches::pagination::{
            BackPaginationOutcome, LoadMoreEventsBackwardsOutcome, PaginatedCache, Pagination,
        },
    },
    ThreadEventCacheInner,
};
use crate::{
    event_cache::caches::pagination::SharedPaginationStatus,
    room::{IncludeRelations, RelationsOptions},
};

/// Intermediate type because the `ThreadEventCache` state is currently owned by
/// `RoomEventCache`.
#[derive(Clone)]
struct ThreadEventCacheWrapper {
    cache: Arc<ThreadEventCacheInner>,

    // Threads do not support pagination status for the moment but we need one, so let's use a
    // dummy one for now.
    dummy_pagination_status: SharedObservable<SharedPaginationStatus>,
}

/// An API object to run pagination queries on a `ThreadEventCache`.
#[allow(missing_debug_implementations)]
pub struct ThreadPagination(Pagination<ThreadEventCacheWrapper>);

impl ThreadPagination {
    /// Construct a new [`ThreadPagination`].
    pub(super) fn new(cache: Arc<ThreadEventCacheInner>) -> Self {
        Self(Pagination::new(ThreadEventCacheWrapper {
            cache,
            dummy_pagination_status: SharedObservable::new(SharedPaginationStatus::Idle {
                hit_timeline_start: false,
            }),
        }))
    }

    /// Starts a back-pagination for the requested number of events.
    ///
    /// This automatically takes care of waiting for a pagination token from
    /// sync, if we haven't done that before.
    ///
    /// It will run multiple back-paginations until one of these two conditions
    /// is met:
    /// - either we've reached the start of the timeline,
    /// - or we've obtained enough events to fulfill the requested number of
    ///   events.
    pub async fn run_backwards_until(
        &self,
        num_requested_events: u16,
    ) -> Result<BackPaginationOutcome> {
        self.0.run_backwards_until(num_requested_events).await
    }

    /// Run a single back-pagination for the requested number of events.
    ///
    /// This automatically takes care of waiting for a pagination token from
    /// sync, if we haven't done that before.
    pub async fn run_backwards_once(&self, batch_size: u16) -> Result<BackPaginationOutcome> {
        self.0.run_backwards_once(batch_size).await
    }
}

impl PaginatedCache for ThreadEventCacheWrapper {
    fn status(&self) -> &SharedObservable<SharedPaginationStatus> {
        &self.dummy_pagination_status
    }

    async fn load_more_events_backwards(&self) -> Result<LoadMoreEventsBackwardsOutcome> {
        let state = self.cache.state.read().await?;

        // If any in-memory chunk is a gap, don't load more events, and let the caller
        // resolve the gap.
        if let Some(prev_token) = state.thread_linked_chunk().rgap().map(|gap| gap.token) {
            trace!(%prev_token, "thread chunk has at least a gap");

            return Ok(LoadMoreEventsBackwardsOutcome::Gap {
                prev_token: Some(prev_token),
                waited_for_initial_prev_token: state.waited_for_initial_prev_token(),
            });
        }

        // If we don't have a gap, then the first event should be the the thread's root;
        // otherwise, we'll restart a pagination from the end.
        if let Some((_pos, event)) = state.thread_linked_chunk().events().next() {
            let first_event_id =
                event.event_id().expect("a linked chunk only stores events with IDs");

            if first_event_id == self.cache.thread_id {
                trace!("thread chunk is fully loaded and non-empty: reached_start=true");

                return Ok(LoadMoreEventsBackwardsOutcome::StartOfTimeline);
            }
        }

        // Otherwise, we don't have a gap nor events. We don't have anything. Poor us.
        // Well, is ok: start a pagination from the end.
        Ok(LoadMoreEventsBackwardsOutcome::Gap {
            prev_token: None,
            waited_for_initial_prev_token: state.waited_for_initial_prev_token(),
        })
    }

    async fn mark_has_waited_for_initial_prev_token(&self) -> Result<()> {
        *self.cache.state.write().await?.waited_for_initial_prev_token_mut() = true;

        Ok(())
    }

    async fn wait_for_prev_token(&self) {
        // TODO: implement once we have persistent storage
    }

    async fn paginate_backwards_with_network(
        &self,
        batch_size: u16,
        prev_token: &Option<String>,
    ) -> Result<Option<(Vec<Event>, Option<String>)>> {
        let Some(room) = self.cache.weak_room.get() else {
            // The client is shutting down.
            return Ok(None);
        };

        let options = RelationsOptions {
            from: prev_token.clone(),
            dir: Direction::Backward,
            limit: Some(batch_size.into()),
            include_relations: IncludeRelations::AllRelations,
            recurse: true,
        };

        let response = room
            .relations(self.cache.thread_id.clone(), options)
            .await
            .map_err(|err| EventCacheError::PaginationError(Arc::new(err)))?;

        Ok(Some((response.chunk, response.next_batch_token)))
    }

    async fn conclude_backwards_pagination_from_disk(
        &self,
        _events: Vec<Event>,
        _timeline_event_diffs: Vec<VectorDiff<Event>>,
        _reached_start: bool,
    ) -> BackPaginationOutcome {
        unimplemented!("loading from disk for threads is not implemented yet");
    }

    async fn conclude_backwards_pagination_from_network(
        &self,
        mut events: Vec<Event>,
        prev_token: Option<String>,
        new_token: Option<String>,
    ) -> Result<Option<BackPaginationOutcome>> {
        let Some(room) = self.cache.weak_room.get() else {
            // The client is shutting down.
            return Ok(None);
        };

        let reached_start = new_token.is_none();

        // Because the state lock is taken again in `load_or_fetch_event`, we need
        // to do this *before* we take the state lock again.
        let root_event = if reached_start {
            // Prepend the thread root event to the results.
            Some(
                room.load_or_fetch_event(&self.cache.thread_id, None)
                    .await
                    .map_err(|err| EventCacheError::PaginationError(Arc::new(err)))?,
            )
        } else {
            None
        };

        let mut state = self.cache.state.write().await?;

        // Save all the events (but the thread root) in the store.
        state.save_events(events.iter().cloned()).await?;

        // Note: the events are still in the reversed order at this point, so
        // pushing will eventually make it so that the root event is the first.
        events.extend(root_event);

        let prev_gap_id = if let Some(token) = prev_token {
            // If the gap id is missing, it means that the gap disappeared during
            // pagination; in this case, early return to the caller.
            let Some(gap_id) = state.thread_linked_chunk().chunk_identifier(|chunk| {
                    matches!(chunk.content(), ChunkContent::Gap(Gap { token: prev_token }) if *prev_token == token)
                }) else {
                    // The previous token has gone missing, so the timeline has been reset in the
                    // meanwhile, but it's fine per this function's contract.
                    return Ok(None);
                };

            Some(gap_id)
        } else {
            None
        };

        // This is a backwards pagination, so the events were returned in the reverse
        // topological order.
        let topo_ordered_events = events.iter().cloned().rev().collect::<Vec<_>>();
        let new_gap = new_token.map(|token| Gap { token });

        let deduplication = state.filter_duplicate_events(topo_ordered_events);

        let (events, new_gap) = if deduplication.non_empty_all_duplicates {
            // If all events are duplicates, we don't need to do anything; ignore
            // the new events and the new gap.
            (Vec::new(), None)
        } else {
            assert!(
                deduplication.in_store_duplicated_event_ids.is_empty(),
                "persistent storage for threads is not implemented yet"
            );
            state.remove_events(deduplication.in_memory_duplicated_event_ids).await?;

            // Keep events and the gap.
            (deduplication.all_events, new_gap)
        };

        // Add the paginated events to the thread chunk.
        let reached_start = state.thread_linked_chunk_mut().push_backwards_pagination_events(
            prev_gap_id,
            new_gap,
            &events,
        );

        state.propagate_changes().await?;

        // Notify observers about the updates.
        let updates = state.thread_linked_chunk_mut().updates_as_vector_diffs();

        if !updates.is_empty() {
            // Send the updates to the listeners.
            let _ = state
                .state
                .sender
                .send(TimelineVectorDiffs { diffs: updates, origin: EventsOrigin::Pagination });
        }

        Ok(Some(BackPaginationOutcome { reached_start, events }))
    }
}
