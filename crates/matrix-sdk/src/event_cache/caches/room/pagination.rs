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

use std::{
    collections::{BTreeMap, HashSet},
    fmt,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::VectorDiff;
use futures_core::{Stream, ready};
use matrix_sdk_base::{
    event_cache::{Event, Gap},
    linked_chunk::{ChunkContent, LinkedChunkId, Position, Update},
};
use pin_project_lite::pin_project;
use ruma::{OwnedEventId, api::Direction};
use tracing::{debug, error, trace};

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
            PaginationMode,
        },
    },
    RoomEventCacheInner, RoomEventCacheUpdate,
    state::RoomEventCacheState,
};
use crate::{
    event_cache::{caches::pagination::SharedPaginationStatus, states::StateLockWriteGuard},
    room::MessagesOptions,
};

pin_project! {
    /// A subscriber to a [`PaginationStatus`].
    ///
    /// This is a manual implementation of a map function on top of an internal type
    /// representing a [`PaginationStatus`].
    pub struct PaginationStatusSubscriber {
        #[pin]
        subscriber: Subscriber<SharedPaginationStatus>,
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for PaginationStatusSubscriber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PaginationStatusSubscriber").finish_non_exhaustive()
    }
}

impl PaginationStatusSubscriber {
    fn map(from: SharedPaginationStatus) -> PaginationStatus {
        match from {
            SharedPaginationStatus::Idle { hit_timeline_start } => {
                PaginationStatus::Idle { hit_timeline_start }
            }
            SharedPaginationStatus::Paginating { .. } => PaginationStatus::Paginating,
        }
    }

    pub fn get(&self) -> PaginationStatus {
        Self::map(self.subscriber.get())
    }

    pub async fn next(&mut self) -> Option<PaginationStatus> {
        self.subscriber.next().await.map(Self::map)
    }

    pub fn next_now(&mut self) -> PaginationStatus {
        Self::map(self.subscriber.next_now())
    }
}

impl Stream for PaginationStatusSubscriber {
    type Item = PaginationStatus;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(ready!(self.project().subscriber.as_mut().poll_next(cx)).map(Self::map))
    }
}

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
    pub(super) fn new(cache: Arc<RoomEventCacheInner>) -> Self {
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
    /// Contrary to [`Self::run_backwards_once`], this never reaches the
    /// network to resolve a gap: gaps encountered while walking the storage
    /// are loaded into memory, surfaced to observers via
    /// [`RoomEventCacheUpdate::UpdateTimelineGaps`], and skipped over, so all
    /// the cached content is reachable even offline. Gaps are resolved on
    /// demand with [`RoomEventCache::resolve_gap`].
    ///
    /// Two exceptions reach the network: an empty room that has never seen
    /// any event still bootstraps over the network (there's no cached content
    /// to show); and once the storage is exhausted, any gap still unresolved
    /// is resolved (one per call) before the start of the timeline is
    /// claimed, since a gap-free chunk head only proves the start once no
    /// gap is left.
    ///
    /// `batch_size` only applies to that network bootstrap; storage loads
    /// whole chunks at a time.
    ///
    /// [`RoomEventCacheUpdate::UpdateTimelineGaps`]: super::RoomEventCacheUpdate::UpdateTimelineGaps
    /// [`RoomEventCache::resolve_gap`]: super::RoomEventCache::resolve_gap
    pub async fn run_backwards_once_from_storage(
        &self,
        batch_size: u16,
    ) -> Result<BackPaginationOutcome> {
        self.0.run_backwards_once(batch_size, PaginationMode::StorageOnly).await
    }

    /// Returns a subscriber to the pagination status.
    pub fn status(&self) -> PaginationStatusSubscriber {
        PaginationStatusSubscriber { subscriber: self.0.cache.status().subscribe() }
    }

    #[cfg(test)]
    pub(super) async fn load_more_events_backwards(
        &self,
    ) -> Result<LoadMoreEventsBackwardsOutcome> {
        self.0.cache.load_more_events_backwards(PaginationMode::StorageThenNetwork).await
    }
}

impl PaginatedCache for Arc<RoomEventCacheInner> {
    fn status(&self) -> &SharedObservable<SharedPaginationStatus> {
        &self.shared_pagination_status
    }

    async fn load_more_events_backwards(
        &self,
        mode: PaginationMode,
    ) -> Result<LoadMoreEventsBackwardsOutcome> {
        let mut state = self.state.write().await?;

        // If any in-memory chunk is a gap, don't load more events, and let the caller
        // resolve the gap.
        //
        // In storage-only mode, gaps aren't resolved by paginations: keep loading
        // previous chunks from the storage past them instead.
        if mode == PaginationMode::StorageThenNetwork
            && let Some(prev_token) = state.room_linked_chunk().rgap().map(|gap| gap.token)
        {
            return Ok(LoadMoreEventsBackwardsOutcome::Gap {
                prev_token: Some(prev_token),
                waited_for_initial_prev_token: state.waited_for_initial_prev_token(),
            });
        }

        let prev_first_chunk = state.room_linked_chunk().first_chunk();

        // Load the first chunk's previous chunk.
        let linked_chunk_id = LinkedChunkId::Room(&state.state.room_id);
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
                // The linked chunk is now fully loaded.
                //
                // If the first chunk is a gap, consider we've reached the start of the
                // timeline as far as the storage is concerned: the gap still needs a
                // manual resolution to make further progress (storage-only mode only;
                // in the historical mode, the `rgap` early-return above fires first).
                if state.room_linked_chunk().first_chunk_as_gap().is_some() {
                    trace!("chunk is fully loaded with a leading gap: reached_start=true");
                    return Ok(LoadMoreEventsBackwardsOutcome::StartOfTimeline);
                }

                // If we never received events for this room, this means we've never received a
                // sync for that room, because every room must have *at least* a room creation
                // event. Otherwise, we have reached the start of the timeline.

                if state.room_linked_chunk().events().next().is_some() {
                    // A gap-free head only means "start of the timeline" once no
                    // gap is left anywhere: gaps get dropped as redundant on the
                    // strength of another gap reaching the same history, so a
                    // leading events chunk can outlive the gap that used to
                    // precede it. Resolve the remaining gaps first (same rule as
                    // `push_backwards_pagination_events`' `!has_gaps`).
                    if let Some(prev_token) = state.room_linked_chunk().first_gap_token() {
                        trace!("chunk is fully loaded but a gap remains: resolving it");
                        return Ok(LoadMoreEventsBackwardsOutcome::ResolveGap { prev_token });
                    }

                    // If there's at least one event, this means we've reached the start of the
                    // timeline, since the chunk is fully loaded.
                    trace!("chunk is fully loaded and non-empty: reached_start=true");
                    return Ok(LoadMoreEventsBackwardsOutcome::StartOfTimeline);
                }

                // Otherwise, start back-pagination from the end of the room.
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

        if let Err(err) = state.room_linked_chunk_mut().insert_new_chunk_as_first(new_first_chunk) {
            error!("error when inserting the previous chunk into its linked chunk: {err}");

            // Clear storage for this room.
            state
                .store
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(&state.state.room_id),
                    vec![Update::Clear],
                )
                .await?;

            // Return the error.
            return Err(err.into());
        }

        // Same rule as above: a gap anywhere means the start hasn't been seen.
        let reached_start = reached_start && state.room_linked_chunk().first_gap_token().is_none();

        // ⚠️ Let's not propagate the updates to the store! We already have these data
        // in the store! Let's drain them.
        let _ = state.room_linked_chunk_mut().store_updates().take();

        // However, we want to get updates as `VectorDiff`s.
        let timeline_event_diffs = state.room_linked_chunk_mut().updates_as_vector_diffs();

        // The loaded chunk may have changed the set of in-memory gaps (either
        // it's a gap itself, or it's an events chunk that becomes the anchor
        // of a previously trailing gap). Let observers know.
        //
        // Only in storage-only mode: the historical mode resolves the gap it
        // just loaded right away over the network, so announcing it would
        // only flash a transient gap at observers. (The change-detection in
        // `take_timeline_gaps_update` is sequenced, so an unannounced
        // load-and-resolve cycle correctly results in no update at all.)
        if mode == PaginationMode::StorageOnly
            && let Some(gaps) = state.take_timeline_gaps_update()
        {
            self.update_sender.send(RoomEventCacheUpdate::UpdateTimelineGaps { gaps }, None);
        }

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
        *self.state.write().await?.waited_for_initial_prev_token_mut() = true;

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
            .map_err(|err| EventCacheError::PaginationError(Arc::new(err)))?;

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
                Some(RoomEventCacheGenericUpdate {
                    room_id: self.room_id.clone(),
                    origin: EventsOrigin::Pagination,
                }),
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

        // Keep a copy of the gap token we queried, to detect a non-advancing
        // dead-end below (empty response whose new token equals this one).
        let queried_prev_token = prev_token.clone();

        // Check that the previous token still exists; otherwise it's a sign that the
        // room's timeline has been cleared.
        let prev_gap_id = if let Some(token) = prev_token {
            // Find the corresponding gap in the in-memory linked chunk.
            let gap_chunk_id = state.room_linked_chunk().chunk_identifier(|chunk| {
                    matches!(chunk.content(), ChunkContent::Gap(Gap { token: prev_token }) if *prev_token == token)
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

        // Whether `/messages` returned no events at all for this gap. Captured
        // before `filter_duplicate_events` consumes `events`. If the server has
        // nothing to fill the gap with, we drop the gap below (instead of
        // re-parking its prev-batch token) so back-pagination can reattach to
        // the events already stored behind the gap.
        let network_returned_no_events = events.is_empty();

        let DeduplicationOutcome {
            all_events: mut events,
            in_memory_duplicated_event_ids,
            in_store_duplicated_event_ids,
            non_empty_all_duplicates: mut all_duplicates,
        } = filter_duplicate_events(
            &state.state.own_user_id,
            &state.store,
            LinkedChunkId::Room(&state.state.room_id),
            state.room_linked_chunk(),
            events,
        )
        .await?;

        // Redundant gap guard. Several gaps can sit in memory at once (storage-only
        // pagination surfaces them all), and each is resolved on demand, so two
        // resolutions can walk overlapping ranges of history. If any event
        // returned for this gap already lives *after* the gap in the linked
        // chunk, a newer gap's resolution has already walked past this gap's
        // position: whatever is older is (or will be) reached through that
        // newer gap's own trailing gap. This gap is redundant: drop it, and
        // insert nothing. Applying the usual "move the duplicates here" logic
        // instead would drag those events *backwards*, in front of the newer
        // gap's frontier, and misorder the timeline (events surfacing before
        // history that's older than them; a dropped leading gap then falsely
        // reveals a "start of the room").
        if let Some(gap_id) = prev_gap_id
            && !in_memory_duplicated_event_ids.is_empty()
        {
            let chunks_after_gap: HashSet<_> = state
                .room_linked_chunk()
                .chunks()
                .skip_while(|chunk| chunk.identifier() != gap_id)
                .skip(1)
                .map(|chunk| chunk.identifier())
                .collect();

            if in_memory_duplicated_event_ids
                .iter()
                .any(|(_, position)| chunks_after_gap.contains(&position.chunk_identifier()))
            {
                debug!(
                    "gap resolution returned events already known after the gap: \
                     the gap is redundant, dropping it"
                );
                all_duplicates = true;
            }
        }

        // Anchored resolution. A gap resolved on top of loaded history (the
        // storage walk loads the chunks behind a gap before the gap is
        // resolved) mostly returns events we already hold before the gap,
        // plus a few the cache missed. The legacy path below removes every
        // duplicate and re-inserts the whole batch in place of the gap:
        // correct, but it takes a run of rendered items away and puts it
        // back, and the timeline can't keep its scroll anchor through that
        // (a visible jump). Instead, when the batch orders the duplicates as
        // we do, keep them in place as anchors: each run of new events is
        // inserted right before the anchor following it in the batch, and
        // the run newer than every anchor takes the gap's place. The batch is
        // a contiguous slice of the room's history, so the gaps sitting
        // between the oldest anchor and the resolved gap hold nothing it
        // didn't return: they are dropped, and so is the token for even
        // older history, which points into the anchors' own chunk. Anything
        // the rule doesn't confidently claim (older new events with a token
        // to follow, disagreeing order) takes the legacy path. (Duplicates
        // *after* the gap were handled just above.)
        let anchored = if let Some(gap_id) = prev_gap_id
            && !all_duplicates
            && !in_memory_duplicated_event_ids.is_empty()
        {
            let by_id = in_memory_duplicated_event_ids
                .iter()
                .cloned()
                .collect::<BTreeMap<OwnedEventId, Position>>();

            // Anchors, in topological (batch) order.
            let anchors = events
                .iter()
                .rev()
                .filter_map(|event| {
                    let event_id = event.event_id()?;
                    Some((event_id.to_owned(), *by_id.get(event_id)?))
                })
                .collect::<Vec<_>>();

            let order_matches = anchors.windows(2).all(|pair| {
                let first = state.room_linked_chunk().event_order(pair[0].1);
                let second = state.room_linked_chunk().event_order(pair[1].1);
                first.zip(second).is_some_and(|(first, second)| first < second)
            });

            // Whether the oldest event of the batch is an anchor: then no
            // new event predates our loaded history, and the new token
            // (older than that anchor) is redundant with what we hold.
            let oldest_is_anchor = events
                .last()
                .and_then(|event| event.event_id())
                .is_some_and(|event_id| by_id.contains_key(event_id));

            (order_matches && (oldest_is_anchor || new_token.is_none())).then_some(anchors)
        } else {
            None
        };

        if let Some(anchors) = anchored {
            debug!(
                num_anchors = anchors.len(),
                num_new = events.len() - anchors.len(),
                "gap resolution anchored on the loaded history before the gap"
            );

            // The store-only duplicates can go: their removal shifts no
            // in-memory position.
            state.remove_events(Vec::new(), in_store_duplicated_event_ids).await?;

            // The gaps between the oldest anchor and the resolved gap are
            // redundant with the batch: drop them.
            let gap_id = prev_gap_id.expect("anchored implies a gap");
            let oldest_anchor_chunk =
                anchors.first().expect("anchors are non-empty").1.chunk_identifier();
            let redundant_gaps = state
                .room_linked_chunk()
                .chunks()
                .skip_while(|chunk| chunk.identifier() != oldest_anchor_chunk)
                .take_while(|chunk| chunk.identifier() != gap_id)
                .filter(|chunk| chunk.is_gap())
                .map(|chunk| chunk.identifier())
                .collect::<Vec<_>>();
            for redundant_gap in redundant_gaps {
                debug!(?redundant_gap, "dropping a gap spanned by the resolved batch");
                if let Err(err) = state.room_linked_chunk_mut().remove_gap_at(redundant_gap) {
                    error!(?err, "gap resolution: failed to drop a redundant gap");
                }
            }

            let anchor_ids =
                anchors.iter().map(|(event_id, _)| event_id.clone()).collect::<HashSet<_>>();

            // Split the batch (topological order) into the runs around the
            // anchors: run `i` goes right before anchor `i`, the trailing
            // run in place of the gap.
            let mut runs = vec![Vec::new()];
            for event in events.iter().rev() {
                if event.event_id().is_some_and(|event_id| anchor_ids.contains(event_id)) {
                    runs.push(Vec::new());
                } else {
                    runs.last_mut().expect("`runs` is never empty").push(event.clone());
                }
            }
            let trailing_run = runs.pop().expect("`runs` is never empty");

            let reached_start = state.room_linked_chunk_mut().push_backwards_pagination_events(
                prev_gap_id,
                None,
                &trailing_run,
            );

            // The position right after the newest anchor (the trailing run's
            // start, or whatever followed the gap): the fallback insertion
            // point below.
            let gap_position = {
                let (_, newest_anchor) = anchors.last().expect("anchors are non-empty");
                let mut found = false;
                state.room_linked_chunk().events().find_map(|(position, _)| {
                    if found {
                        Some(position)
                    } else {
                        found = position == *newest_anchor;
                        None
                    }
                })
            };

            // Deepest position first, so the outstanding (strictly smaller)
            // positions stay valid.
            for ((_, anchor_position), run) in anchors.iter().zip(runs).rev() {
                if !run.is_empty()
                    && let Err(err) = state
                        .room_linked_chunk_mut()
                        .insert_events_at(*anchor_position, run.clone())
                {
                    // Losing the placement is cosmetic, losing the events is
                    // not: put the run where the gap was instead.
                    error!(?err, "gap resolution: stale anchor position; inserting at the gap");
                    if let Some(position) = gap_position {
                        let _ = state.room_linked_chunk_mut().insert_events_at(position, run);
                    }
                }
            }

            let new_events = events
                .iter()
                .rev()
                .filter(|event| {
                    !event.event_id().is_some_and(|event_id| anchor_ids.contains(event_id))
                })
                .cloned()
                .collect::<Vec<_>>();
            state.post_process_new_events(new_events, None).await?;

            send_pagination_updates(self, &mut state);

            let events = events
                .into_iter()
                .filter(|event| {
                    !event.event_id().is_some_and(|event_id| anchor_ids.contains(event_id))
                })
                .collect();

            return Ok(Some(BackPaginationOutcome { events, reached_start }));
        }

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

        // Dead-end gap guard: if `/messages` returned no events AND its new
        // prev-batch token is the same one we just queried, the gap is a
        // non-advancing dead end - re-parking it would loop forever, refetching
        // the same empty page and leaving any events stored behind the gap
        // stranded (observed after a "limited" sliding sync collapses the live
        // timeline: the server keeps returning an empty chunk with the same
        // token). Drop the gap (reusing the all-duplicates gap-removal path
        // above) so the next back-pagination reattaches to the stored events.
        //
        // An empty response with a *different*, advancing token is legitimate
        // (e.g. a page that contained only filtered-out events) - keep that gap
        // and follow the new token.
        if network_returned_no_events && new_token == queried_prev_token {
            new_token = None;
        }

        // `/messages` has been called with `dir=b` (backwards), so the events are in
        // the inverted order; reorder them.
        let topo_ordered_events = events.iter().rev().cloned().collect::<Vec<_>>();

        let new_gap = new_token.as_ref().map(|prev_token| Gap { token: prev_token.clone() });
        let reached_start = state.room_linked_chunk_mut().push_backwards_pagination_events(
            prev_gap_id,
            new_gap,
            &topo_ordered_events,
        );

        // A back-pagination can't include new read receipt events, as those are
        // ephemeral events not included in /messages responses, so we can
        // safely set the receipt event to None here.
        //
        // Note: read receipts may be updated anyhow in the post-processing step, as the
        // back-pagination may have revealed the event pointed to by the latest read
        // receipt.
        let receipt_event = None;

        // Note: this flushes updates to the store.
        state.post_process_new_events(topo_ordered_events, receipt_event).await?;

        send_pagination_updates(self, &mut state);

        Ok(Some(BackPaginationOutcome { events, reached_start }))
    }
}

/// Send the pending timeline diffs (and gaps update, if observers are told
/// about gaps) after a network back-pagination.
fn send_pagination_updates(
    inner: &RoomEventCacheInner,
    state: &mut StateLockWriteGuard<'_, RoomEventCacheState>,
) {
    let timeline_event_diffs = state.room_linked_chunk_mut().updates_as_vector_diffs();

    if !timeline_event_diffs.is_empty() {
        inner.update_sender.send(
            RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                diffs: timeline_event_diffs,
                origin: EventsOrigin::Pagination,
            }),
            Some(RoomEventCacheGenericUpdate {
                room_id: inner.room_id.clone(),
                origin: EventsOrigin::Pagination,
            }),
        );
    }

    // The resolved gap has been removed (and possibly replaced with a new
    // one carrying the next prev-batch token): let observers know — but
    // only if they've been told about gaps before. A purely legacy
    // pagination flow (which loads gaps only to resolve them right away,
    // and may park a new one for its own next run) shouldn't start
    // announcing gaps on its own; if observers believe there are no gaps,
    // leave it that way, the next sync or storage pagination reconciles.
    if state.has_announced_timeline_gaps()
        && let Some(gaps) = state.take_timeline_gaps_update()
    {
        inner.update_sender.send(RoomEventCacheUpdate::UpdateTimelineGaps { gaps }, None);
    }
}

impl fmt::Debug for RoomPagination {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("RoomPagination").finish_non_exhaustive()
    }
}
