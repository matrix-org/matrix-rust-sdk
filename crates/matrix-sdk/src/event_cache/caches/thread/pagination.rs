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
    linked_chunk::{ChunkContent, LinkedChunkId, Update},
};
use ruma::api::Direction;
use tracing::{error, trace};

use super::{
    super::{
        super::{
            EventCacheError, EventsOrigin, Result, TimelineVectorDiffs,
            deduplicator::{DeduplicationOutcome, filter_duplicate_events},
        },
        pagination::{
            BackPaginationOutcome, LoadMoreEventsBackwardsOutcome, PaginatedCache, Pagination,
        },
    },
    ThreadEventCacheInner,
};
use crate::{
    event_cache::caches::pagination::SharedPaginationStatus,
    room::{IncludeRelations, RelationsOptions},
};

/// Intermediate type because the `ThreadEventCache` state doesn't provide all
/// the feature for the moment.
///
/// TODO: Remove this intermediate type.
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
        let mut state = self.cache.state.write().await?;

        // If any in-memory chunk is a gap, don't load more events, and let the caller
        // resolve the gap.
        if let Some(prev_token) = state.thread_linked_chunk().rgap().map(|gap| gap.token) {
            trace!(%prev_token, "thread chunk has at least a gap");

            return Ok(LoadMoreEventsBackwardsOutcome::Gap {
                prev_token: Some(prev_token),
                waited_for_initial_prev_token: state.waited_for_initial_prev_token(),
            });
        }

        let prev_first_chunk =
            state.thread_linked_chunk().chunks().next().expect("a linked chunk is never empty");

        // The first chunk is not a gap, we can load its previous chunk.
        let linked_chunk_id = LinkedChunkId::Thread(&state.room_id, &state.thread_id);
        let new_first_chunk = match state
            .store
            .load_previous_chunk(linked_chunk_id, prev_first_chunk.identifier())
            .await
        {
            Ok(Some(new_first_chunk)) => {
                // All good, let's continue with this chunk.
                new_first_chunk
            }

            Ok(None) => {
                // No previous chunk in the store.
                //
                // If the first in-memory event is the thread root, it's all good, we have
                // effectively reached the start of the thread.
                if let Some((_pos, first_event)) = state.thread_linked_chunk().events().next()
                    && self.cache.thread_id
                        == first_event.event_id().expect("Stored events all have an ID")
                {
                    trace!("thread chunk is fully loaded and non-empty: reached_start=true");

                    return Ok(LoadMoreEventsBackwardsOutcome::StartOfTimeline);
                }

                // Otherwise, start back-pagination from the end of the thread.
                return Ok(LoadMoreEventsBackwardsOutcome::Gap {
                    prev_token: None,
                    waited_for_initial_prev_token: state.waited_for_initial_prev_token(),
                });
            }

            Err(err) => {
                error!("error when loading the previous chunk of a linked chunk: {err}");

                // Clear storage for this room.
                state
                    .store
                    .handle_linked_chunk_updates(linked_chunk_id, vec![Update::Clear])
                    .await?;

                // Return the error.
                return Err(err.into());
            }
        };

        let chunk_content = new_first_chunk.content.clone();

        // We've reached the start on disk, if and only if, there was no chunk prior to
        // the one we just loaded.
        //
        // This value is correct, if and only if, it is used for a chunk content of kind
        // `Items`.
        let reached_start = new_first_chunk.previous.is_none();

        if let Err(err) = state.thread_linked_chunk_mut().insert_new_chunk_as_first(new_first_chunk)
        {
            error!("error when inserting the previous chunk into its linked chunk: {err}");

            // Clear storage for this thread.
            state
                .store
                .handle_linked_chunk_updates(
                    LinkedChunkId::Thread(&state.state.room_id, &state.state.thread_id),
                    vec![Update::Clear],
                )
                .await?;

            // Return the error.
            return Err(err.into());
        }

        // ⚠️ Let's not propagate the updates to the store! We already have these data
        // in the store! Let's drain them.
        let _ = state.thread_linked_chunk_mut().store_updates().take();

        // However, we want to get updates as `VectorDiff`s.
        let timeline_event_diffs = state.thread_linked_chunk_mut().updates_as_vector_diffs();

        Ok(match chunk_content {
            ChunkContent::Gap(gap) => {
                trace!("reloaded chunk from disk (gap)");

                LoadMoreEventsBackwardsOutcome::Gap {
                    prev_token: Some(gap.token),
                    waited_for_initial_prev_token: state.waited_for_initial_prev_token(),
                }
            }

            ChunkContent::Items(events) => {
                trace!(?reached_start, "reloaded chunk from disk ({} items)", events.len());

                LoadMoreEventsBackwardsOutcome::Events {
                    events,
                    timeline_event_diffs,
                    reached_start,
                }
            }
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
        events: Vec<Event>,
        timeline_event_diffs: Vec<VectorDiff<Event>>,
        reached_start: bool,
    ) -> BackPaginationOutcome {
        if !timeline_event_diffs.is_empty() {
            self.cache.update_sender.send(TimelineVectorDiffs {
                diffs: timeline_event_diffs,
                origin: EventsOrigin::Cache,
            });
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

        let DeduplicationOutcome {
            all_events: events,
            in_memory_duplicated_event_ids,
            in_store_duplicated_event_ids,
            non_empty_all_duplicates: all_duplicates,
        } = filter_duplicate_events(
            &state.state.own_user_id,
            &state.store,
            LinkedChunkId::Thread(&state.state.room_id, &state.state.thread_id),
            state.thread_linked_chunk(),
            topo_ordered_events,
        )
        .await?;

        let (events, new_gap) = if all_duplicates {
            // If all events are duplicates, we don't need to do anything; ignore
            // the new events and the new gap.
            (Vec::new(), None)
        } else {
            state
                .remove_events(in_memory_duplicated_event_ids, in_store_duplicated_event_ids)
                .await?;

            // Keep events and the gap.
            (events, new_gap)
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
            let _ = self
                .cache
                .update_sender
                .send(TimelineVectorDiffs { diffs: updates, origin: EventsOrigin::Pagination });
        }

        Ok(Some(BackPaginationOutcome { reached_start, events }))
    }
}
