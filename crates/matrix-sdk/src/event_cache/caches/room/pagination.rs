// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! The [`RoomPagination`] type makes it possible to paginate a
//! [`RoomEventCache`].
//!
//! [`RoomEventCache`]: super::super::super::RoomEventCache

use std::sync::Arc;

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    event_cache::{Event, Gap},
    linked_chunk::{ChunkContent, LinkedChunkId},
};
use ruma::api::Direction;

pub use super::super::pagination::PaginationStatus;
use super::{
    super::{
        super::{
            EventCacheError, EventsOrigin, Result, RoomEventCacheGenericUpdate,
            deduplicator::{DeduplicationOutcome, filter_duplicate_events},
        },
        TimelineVectorDiffs,
        pagination::{
            BackPaginationOutcome, LoadMoreEventsBackwardsOutcome, PaginatedCache, Pagination,
        },
    },
    PostProcessingOrigin, RoomEventCacheInner, RoomEventCacheUpdate,
};
use crate::room::MessagesOptions;

/// An API object to run pagination queries on a [`RoomEventCache`].
///
/// Can be created with [`RoomEventCache::pagination()`].
///
/// [`RoomEventCache`]: super::super::super::RoomEventCache
/// [`RoomEventCache::pagination()`]: super::super::super::RoomEventCache::pagination
#[allow(missing_debug_implementations)]
#[derive(Clone)]
pub struct RoomPagination(Pagination<Arc<RoomEventCacheInner>>);

impl RoomPagination {
    /// Construct a new [`RoomPagination`].
    pub(in super::super::super) fn new(cache: Arc<RoomEventCacheInner>) -> Self {
        Self(Pagination::new(cache))
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

    /// Returns a subscriber to the pagination status.
    pub fn status(&self) -> Subscriber<PaginationStatus> {
        self.0.cache.status().subscribe()
    }
}

impl PaginatedCache for Arc<RoomEventCacheInner> {
    fn status(&self) -> &SharedObservable<PaginationStatus> {
        &self.pagination_status
    }

    async fn load_more_events_backwards(&self) -> Result<LoadMoreEventsBackwardsOutcome> {
        self.state.write().await?.load_more_events_backwards().await
    }

    async fn mark_has_waited_for_initial_prev_token(&self) -> Result<()> {
        *self.state.write().await?.waited_for_initial_prev_token() = true;

        Ok(())
    }

    async fn wait_for_prev_token(&self) {
        self.pagination_batch_token_notifier.notified().await
    }

    async fn paginate_backwards_with_network(
        &self,
        batch_size: u16,
        prev_token: &Option<String>,
    ) -> Result<Option<(Vec<Event>, Option<String>)>> {
        let Some(room) = self.weak_room.get() else {
            // The client is shutting down.
            return Ok(None);
        };

        let mut options = MessagesOptions::new(Direction::Backward).from(prev_token.as_deref());
        options.limit = batch_size.into();

        let response = room
            .messages(options)
            .await
            .map_err(|err| EventCacheError::BackpaginationError(Box::new(err)))?;

        Ok(Some((response.chunk, response.end)))
    }

    async fn conclude_backwards_pagination_from_disk(
        &self,
        events: Vec<Event>,
        timeline_event_diffs: Vec<VectorDiff<Event>>,
        reached_start: bool,
    ) -> BackPaginationOutcome {
        if !timeline_event_diffs.is_empty() {
            self.update_sender.send(
                RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                    diffs: timeline_event_diffs,
                    origin: EventsOrigin::Cache,
                }),
                Some(RoomEventCacheGenericUpdate { room_id: self.room_id.clone() }),
            );
        }

        BackPaginationOutcome {
            reached_start,
            // This is a backwards pagination. `BackPaginationOutcome` expects events to
            // be in “reverse order”.
            events: events.into_iter().rev().collect(),
        }
    }

    async fn conclude_backwards_pagination_from_network(
        &self,
        events: Vec<Event>,
        prev_token: Option<String>,
        mut new_token: Option<String>,
    ) -> Result<Option<BackPaginationOutcome>> {
        let mut state = self.state.write().await?;

        // Check that the previous token still exists; otherwise it's a sign that the
        // room's timeline has been cleared.
        let prev_gap_id = if let Some(token) = prev_token {
            // Find the corresponding gap in the in-memory linked chunk.
            let gap_chunk_id = state.room_linked_chunk().chunk_identifier(|chunk| {
                    matches!(chunk.content(), ChunkContent::Gap(Gap { prev_token }) if *prev_token == token)
                });

            if gap_chunk_id.is_none() {
                // We got a previous-batch token from the linked chunk *before* running the
                // request, but it is missing *after* completing the request.
                //
                // It may be a sign the linked chunk has been reset, but it's fine!
                return Ok(None);
            }

            gap_chunk_id
        } else {
            None
        };

        let DeduplicationOutcome {
            all_events: mut events,
            in_memory_duplicated_event_ids,
            in_store_duplicated_event_ids,
            non_empty_all_duplicates: all_duplicates,
        } = {
            let room_linked_chunk = state.room_linked_chunk();

            filter_duplicate_events(
                &state.state.own_user_id,
                &state.store,
                LinkedChunkId::Room(&state.state.room_id),
                room_linked_chunk,
                events,
            )
            .await?
        };

        // If not all the events have been back-paginated, we need to remove the
        // previous ones, otherwise we can end up with misordered events.
        //
        // Consider the following scenario:
        // - sync returns [D, E, F]
        // - then sync returns [] with a previous batch token PB1, so the internal
        //   linked chunk state is [D, E, F, PB1].
        // - back-paginating with PB1 may return [A, B, C, D, E, F].
        //
        // Only inserting the new events when replacing PB1 would result in a timeline
        // ordering of [D, E, F, A, B, C], which is incorrect. So we do have to remove
        // all the events, in case this happens (see also #4746).

        if !all_duplicates {
            // Let's forget all the previous events.
            state
                .remove_events(in_memory_duplicated_event_ids, in_store_duplicated_event_ids)
                .await?;
        } else {
            // All new events are duplicated, they can all be ignored.
            events.clear();
            // The gap can be ditched too, as it won't be useful to backpaginate any
            // further.
            new_token = None;
        }

        // `/messages` has been called with `dir=b` (backwards), so the events are in
        // the inverted order; reorder them.
        let topo_ordered_events = events.iter().rev().cloned().collect::<Vec<_>>();

        let new_gap = new_token.map(|prev_token| Gap { prev_token });
        let reached_start = state.room_linked_chunk_mut().push_backwards_pagination_events(
            prev_gap_id,
            new_gap,
            &topo_ordered_events,
        );

        // Note: this flushes updates to the store.
        state
            .post_process_new_events(topo_ordered_events, PostProcessingOrigin::Backpagination)
            .await?;

        let timeline_event_diffs = state.room_linked_chunk_mut().updates_as_vector_diffs();

        if !timeline_event_diffs.is_empty() {
            self.update_sender.send(
                RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                    diffs: timeline_event_diffs,
                    origin: EventsOrigin::Pagination,
                }),
                Some(RoomEventCacheGenericUpdate { room_id: self.room_id.clone() }),
            );
        }

        Ok(Some(BackPaginationOutcome { events, reached_start }))
    }
}
