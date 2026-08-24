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

use std::{fmt, sync::Arc};

use eyeball::SharedObservable;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    event_cache::{Event, Gap},
    linked_chunk::{ChunkContent, ChunkIdentifier, LinkedChunkId, Update},
};
use ruma::{EventId, api::Direction};
use tracing::{error, info, trace};

use super::{
    super::{
        super::{
            EventCacheError, EventsOrigin, Result,
            deduplicator::{DeduplicationOutcome, filter_duplicate_events},
        },
        event_linked_chunk::EventLinkedChunk,
        pagination::{
            BackPaginationOutcome, LoadMoreEventsBackwardsOutcome, PaginatedCache, Pagination,
            PaginationMode, SharedPaginationStatus,
        },
        room::RoomEventCacheGenericUpdate,
    },
    ThreadEventCacheInner,
};
use crate::room::{IncludeRelations, RelationsOptions};

/// Intermediate type because the `ThreadEventCache` state doesn't provide all
/// the feature for the moment.
//
// TODO: Remove this intermediate type.
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
        self.0.run_backwards_until(num_requested_events, PaginationMode::StorageThenNetwork).await
    }

    /// Run a single back-pagination for the requested number of events.
    ///
    /// This automatically takes care of waiting for a pagination token from
    /// sync, if we haven't done that before.
    pub async fn run_backwards_once(&self, batch_size: u16) -> Result<BackPaginationOutcome> {
        self.0.run_backwards_once(batch_size, PaginationMode::StorageThenNetwork).await
    }

    /// Run a single back-pagination from the storage only.
    ///
    /// Contrary to [`Self::run_backwards_once`], gaps met while walking the
    /// storage aren't resolved: they're loaded into memory, exposed via
    /// [`ThreadEventCache::timeline_gaps`] (observers get an update when the
    /// set changes, possibly one without any event diff), and skipped over,
    /// so all the cached content is reachable without touching the network.
    /// Gaps are resolved on demand with [`ThreadEventCache::resolve_gap`].
    ///
    /// Once the storage is exhausted, the network is reached after all: the
    /// gaps still unresolved are resolved (oldest first, one per call) before
    /// the start of the thread is claimed, and a thread whose oldest known
    /// event isn't the thread root paginates from the end of the thread
    /// (parking the pagination token in front of its oldest known event in
    /// between calls), until the root is known. `reached_start` is only ever
    /// returned with the thread root leading the thread and no gap left.
    ///
    /// `batch_size` only applies to network requests; storage loads whole
    /// chunks at a time.
    ///
    /// [`ThreadEventCache::timeline_gaps`]: super::ThreadEventCache::timeline_gaps
    /// [`ThreadEventCache::resolve_gap`]: super::ThreadEventCache::resolve_gap
    pub async fn run_backwards_once_from_storage(
        &self,
        batch_size: u16,
    ) -> Result<BackPaginationOutcome> {
        self.0.run_backwards_once(batch_size, PaginationMode::StorageOnly).await
    }

    /// Resolve the gap identified by the given prev-batch token, with a single
    /// `/relations` request to the server.
    ///
    /// See [`RoomEventCache::resolve_gap`] for the contract, which is the
    /// same, with [`TimelineVectorDiffs`] updates (and the gaps snapshot
    /// pulled alongside them) in lieu of the room's updates.
    ///
    /// [`RoomEventCache::resolve_gap`]: super::super::RoomEventCache::resolve_gap
    /// [`TimelineVectorDiffs`]: super::super::TimelineVectorDiffs
    pub async fn resolve_gap(&self, prev_token: String, batch_size: u16) -> Result<bool> {
        let wrapper = &self.0.cache;

        // Deduplicate concurrent resolutions of the same gap.
        let Some(_in_flight) = wrapper.cache.gap_resolutions_in_flight.begin(&prev_token) else {
            info!("THREADPAG resolve: gap resolution already in flight, skipping");
            return Ok(false);
        };

        // Cheap pre-check that the gap (still) exists, to save a network
        // round-trip on stale resolution requests. The post-request check in
        // `conclude_backwards_pagination_from_network` remains authoritative.
        if !wrapper
            .cache
            .state
            .read()
            .await?
            .timeline_gaps()
            .iter()
            .any(|gap| gap.prev_token == prev_token)
        {
            info!("THREADPAG resolve: gap is unknown (already resolved?), skipping");
            return Ok(false);
        }

        let prev_token = Some(prev_token);

        let Some((events, new_token)) =
            wrapper.paginate_backwards_with_network(batch_size, &prev_token).await?
        else {
            // The client is shutting down.
            return Ok(false);
        };

        Ok(wrapper
            .conclude_backwards_pagination_from_network(events, prev_token, new_token)
            .await?
            .is_some())
    }
}

/// Whether the given gap chunk leads the loaded part of the thread, i.e. no
/// event is loaded before it.
fn gap_leads(thread_linked_chunk: &EventLinkedChunk, gap_id: ChunkIdentifier) -> bool {
    for chunk in thread_linked_chunk.chunks() {
        if chunk.identifier() == gap_id {
            return true;
        }
        if matches!(chunk.content(), ChunkContent::Items(items) if !items.is_empty()) {
            return false;
        }
    }
    false
}

/// Whether the thread root leads the loaded part of the thread, i.e. is its
/// oldest known event.
fn root_leads(thread_linked_chunk: &EventLinkedChunk, thread_id: &EventId) -> bool {
    thread_linked_chunk
        .events()
        .next()
        .is_some_and(|(_pos, first_event)| first_event.event_id().as_deref() == Some(thread_id))
}

impl PaginatedCache for ThreadEventCacheWrapper {
    fn status(&self) -> &SharedObservable<SharedPaginationStatus> {
        &self.dummy_pagination_status
    }

    async fn load_more_events_backwards(
        &self,
        mode: PaginationMode,
    ) -> Result<LoadMoreEventsBackwardsOutcome> {
        let mut state = self.cache.state.write().await?;

        // Threads don't wait for an initial prev-batch token from sync: unlike a
        // room's, a thread's token only ever comes with new activity in that
        // very thread, so the wait would mostly time out. Paginating from the
        // end of the thread right away is safe (see
        // `conclude_backwards_pagination_from_network`).
        let waited_for_initial_prev_token = true;

        // If any in-memory chunk is a gap, don't load more events, and let the caller
        // resolve the gap.
        //
        // In storage-only mode, gaps aren't resolved by paginations: keep loading
        // previous chunks from the storage past them instead.
        if mode == PaginationMode::StorageThenNetwork
            && let Some(prev_token) = state.thread_linked_chunk().rgap().map(|gap| gap.token)
        {
            trace!(%prev_token, "thread chunk has at least a gap");

            return Ok(LoadMoreEventsBackwardsOutcome::Gap {
                prev_token: Some(prev_token),
                waited_for_initial_prev_token,
            });
        }

        let prev_first_chunk = state.thread_linked_chunk().first_chunk();

        // If we are here, it means all gaps have been resolved (see the `if` block
        // above), or that we're walking the storage past them. Load the first
        // chunk's previous chunk.
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
                // No previous chunk in the store: the linked chunk is fully loaded.
                //
                // A leading gap is resolved on demand through its gap item, like
                // the room's: as far as the storage is concerned this is the start
                // of the thread. Resolving it from here as well put a second
                // spinner on screen (the pagination indicator above the gap item,
                // round 33). Storage-only mode only: in the historical mode, the
                // `rgap` early-return above fires first.
                if state.thread_linked_chunk().first_chunk_as_gap().is_some() {
                    info!("THREADPAG load: fully loaded with a leading gap: reached_start=true");

                    return Ok(LoadMoreEventsBackwardsOutcome::StartOfTimeline);
                }

                // A gap anywhere else means some of the thread is still unknown: resolve
                // the oldest one first (storage-only mode walked past it; in the
                // historical mode, the `rgap` early-return above fires first).
                // Redundant gaps get dropped on the strength of another gap
                // reaching the same history, so even a root-led chunk proves the
                // start only once no gap is left (same rule as
                // `push_backwards_pagination_events`' `!has_gaps`).
                if let Some(prev_token) = state.thread_linked_chunk().first_gap_token() {
                    info!("THREADPAG load: fully loaded but a gap remains: resolving it");

                    return Ok(LoadMoreEventsBackwardsOutcome::ResolveGap { prev_token });
                }

                // If the first in-memory event is the thread root, it's all good, we have
                // effectively reached the start of the thread.
                if root_leads(state.thread_linked_chunk(), &self.cache.thread_id) {
                    info!(
                        "THREADPAG load: fully loaded and starts with the root: reached_start=true"
                    );

                    return Ok(LoadMoreEventsBackwardsOutcome::StartOfTimeline);
                }

                // Otherwise, start back-pagination from the end of the thread.
                info!("THREADPAG load: fully loaded, rootless and gap-free: paginating from the end");
                return Ok(LoadMoreEventsBackwardsOutcome::Gap {
                    prev_token: None,
                    waited_for_initial_prev_token,
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
        let loaded_chunk_id = new_first_chunk.identifier;

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
                    LinkedChunkId::Thread(&state.room_id, &state.thread_id),
                    vec![Update::Clear],
                )
                .await?;

            // Return the error.
            return Err(err.into());
        }

        // Same rules as above: a gap anywhere, or a head that isn't the thread
        // root, means the start hasn't been seen.
        let reached_start = reached_start
            && state.thread_linked_chunk().first_gap_token().is_none()
            && root_leads(state.thread_linked_chunk(), &self.cache.thread_id);

        // ⚠️ Let's not propagate the updates to the store! We already have these data
        // in the store! Let's drain them.
        let _ = state.thread_linked_chunk_mut().store_updates().take();

        // A gap that sits before the thread root is provably empty: nothing
        // precedes a thread's root event. If we just loaded such a gap while the
        // root already leads the known events, drop it (and persist the drop,
        // healing the stored chunk) instead of surfacing a spinner before the
        // root and issuing a doomed `/relations` to resolve it. Without this, a
        // thread that was fully loaded once shows a spinner before its root on
        // every reopen, and burns a `/relations` round-trip on it.
        if matches!(chunk_content, ChunkContent::Gap(_))
            && root_leads(state.thread_linked_chunk(), &self.cache.thread_id)
        {
            info!("THREADPAG load: dropping a gap that sits before the thread root");

            if let Err(err) = state.thread_linked_chunk_mut().remove_gap_at(loaded_chunk_id) {
                // Non-fatal: fall through to the normal path (surface the gap),
                // which is what would have happened without this shortcut.
                error!(?err, "failed to drop the gap before the thread root");
            } else {
                // Persist the removal so the stored chunk is healed for next
                // time, then drain the resulting update (nothing was announced
                // for this gap, so observers need no diff).
                state.state.propagate_changes(&state.store).await?;
                let _ = state.thread_linked_chunk_mut().store_updates().take();

                return Ok(LoadMoreEventsBackwardsOutcome::Events {
                    events: Vec::new(),
                    timeline_event_diffs: Vec::new(),
                    reached_start: true,
                });
            }
        }

        let timeline_event_diffs = state.thread_linked_chunk_mut().updates_as_vector_diffs();

        state.send_timeline_updates(
            timeline_event_diffs,
            EventsOrigin::Cache,
            Some(RoomEventCacheGenericUpdate {
                room_id: self.cache.room_id.clone(),
                origin: EventsOrigin::Pagination,
            }),
        );

        Ok(match chunk_content {
            ChunkContent::Gap(gap) => {
                info!("THREADPAG load: reloaded chunk from disk (gap)");

                LoadMoreEventsBackwardsOutcome::Gap {
                    prev_token: Some(gap.token),
                    waited_for_initial_prev_token,
                }
            }

            ChunkContent::Items(events) => {
                info!(?reached_start, "THREADPAG load: reloaded chunk from disk ({} items)", events.len());

                LoadMoreEventsBackwardsOutcome::Events {
                    events,
                    // Already sent above.
                    timeline_event_diffs: Vec::new(),
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
        self.cache.pagination_batch_token_notifier.notified().await
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
        // The diffs have been sent by `load_more_events_backwards` already.
        debug_assert!(timeline_event_diffs.is_empty());

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
        mut new_token: Option<String>,
    ) -> Result<Option<BackPaginationOutcome>> {
        let Some(room) = self.cache.weak_room.get() else {
            // The client is shutting down.
            return Ok(None);
        };

        let from_the_end = prev_token.is_none();
        let network_returned_no_events = events.is_empty();

        // The thread root event is **NOT** part of the `/relations` response.
        // However, we want the thread root event to be part of the thread itself. It's
        // easier in a lot of situations. Let's load it if necessary.
        //
        // It is necessary to load the thread root event when `new_token` is `None`,
        // i.e. when we've reached the start of the thread usually.
        //
        // We must do this dance before acquiring the state lock because
        // `Room::load_or_fetch_event` is hitting the state lock too.
        if new_token.is_none() {
            events.push(
                room.load_or_fetch_event(&self.cache.thread_id, None)
                    .await
                    .map_err(|err| EventCacheError::PaginationError(Arc::new(err)))?,
            );
        }

        let mut state = self.cache.state.write().await?;

        // Check that the previous token still exists; otherwise it's a sign that the
        // thread's timeline has been cleared.
        let prev_gap_id = if let Some(token) = &prev_token {
            // Find the corresponding gap in the in-memory linked chunk.
            let gap_chunk_id = state.thread_linked_chunk().chunk_identifier(|chunk| {
                    matches!(chunk.content(), ChunkContent::Gap(Gap { token: prev_token }) if prev_token == token)
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
        } = filter_duplicate_events(
            &state.own_user_id,
            &state.store,
            LinkedChunkId::Thread(&state.room_id, &state.thread_id),
            state.thread_linked_chunk(),
            events,
        )
        .await?;

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
        } else if prev_gap_id.is_some_and(|gap_id| gap_leads(state.thread_linked_chunk(), gap_id))
            && !root_leads(state.thread_linked_chunk(), &self.cache.thread_id)
        {
            // All the events are duplicated, but the resolved gap LEADS the
            // thread and the root doesn't: the overlap only proves the known
            // events connect to the gap's position, not that the history
            // behind the token has been seen - in particular the thread root,
            // which `/relations` never returns and which is only fetched once
            // a page comes back without a next-batch token. Ditching the gap
            // here would strand the thread: the leading gap's item was the
            // affordance for reaching the rest of the thread (clients stop
            // paginating once the start was claimed), so nothing would ever
            // fetch the root. Keep walking instead: migrate the duplicates
            // into the gap's position (keeping their order) and follow the
            // token; the final page brings the root to the head.
            info!(
                remaining_token = ?new_token,
                "THREADPAG conclude: all-duplicates page on the leading gap, keeping the token"
            );
            state
                .remove_events(in_memory_duplicated_event_ids, in_store_duplicated_event_ids)
                .await?;
        } else {
            // All new events are duplicated, they can all be ignored.
            events.clear();

            // The gap can be ditched too, as it won't be useful to backpaginate any
            // further… unless this was a pagination from the end of the thread: a
            // page of known events there only proves the *newest* events are
            // known, not the older ones, so keep following the token (parked in
            // front of the oldest known event below): the next page is the first
            // with a chance to bring unknown events, or the thread root. Dropping
            // it would claim the start of the thread at the oldest known event.
            if !from_the_end {
                new_token = None;
            }
        }

        // Dead-end gap guard: an empty page that hands back the very token we
        // just queried would loop forever if re-parked; drop the gap instead
        // (same as the room's back-pagination).
        if network_returned_no_events && new_token == prev_token {
            new_token = None;
        }

        // `/relations` has been called with `dir=b` (backwards), so the events are in
        // the inverted order; reorder them.
        let topo_ordered_events = events.iter().rev().cloned().collect::<Vec<_>>();

        info!(
            ?prev_token,
            ?new_token,
            from_the_end,
            all_duplicates,
            num_events = events.len(),
            "THREADPAG conclude: applying a network pagination"
        );

        let new_gap = new_token.map(|prev_token| Gap { token: prev_token });
        let reached_start = state.thread_linked_chunk_mut().push_backwards_pagination_events(
            prev_gap_id,
            new_gap,
            &topo_ordered_events,
        );

        // A gap-free, fully loaded linked chunk only means "start of the thread"
        // if the thread root leads it: a gap dropped as redundant may leave a
        // root-less head behind, to be completed by paginating from the end.
        let reached_start =
            reached_start && root_leads(state.thread_linked_chunk(), &self.cache.thread_id);

        // Update the store.
        state.state.propagate_changes(&state.store).await?;

        // Post-process newly inserted events.
        state.post_process_upserted_events(topo_ordered_events.iter()).await?;

        // Notify observers about the updates.
        let timeline_event_diffs = state.thread_linked_chunk_mut().updates_as_vector_diffs();

        state.send_timeline_updates(
            timeline_event_diffs,
            EventsOrigin::Pagination,
            Some(RoomEventCacheGenericUpdate {
                room_id: state.room_id.clone(),
                origin: EventsOrigin::Pagination,
            }),
        );

        info!(reached_start, "THREADPAG conclude: done");

        Ok(Some(BackPaginationOutcome { reached_start, events }))
    }
}

impl fmt::Debug for ThreadPagination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ThreadPagination").finish_non_exhaustive()
    }
}
