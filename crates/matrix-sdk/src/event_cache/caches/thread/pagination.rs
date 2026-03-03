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
use matrix_sdk_base::event_cache::Event;
use ruma::{OwnedEventId, api::Direction};

pub use super::super::pagination::PaginationStatus;
use super::super::{
    super::{
        EventCacheError, Result,
        caches::pagination::{
            BackPaginationOutcome, LoadMoreEventsBackwardsOutcome, PaginatedCache, Pagination,
        },
    },
    room::RoomEventCacheInner,
};
use crate::room::{IncludeRelations, RelationsOptions};

/// Intermediate type because the `ThreadEventCache` state is currently owned by
/// `RoomEventCache`.
struct ThreadEventCacheWrapper {
    cache: Arc<RoomEventCacheInner>,
    thread_id: OwnedEventId,
    // Threads do not support pagination status for the moment but we need one, so let's use a
    // dummy one for now.
    dummy_pagination_status: SharedObservable<PaginationStatus>,
}

/// An API object to run pagination queries on a `ThreadEventCache`.
#[allow(missing_debug_implementations)]
pub struct ThreadPagination(Pagination<ThreadEventCacheWrapper>);

impl ThreadPagination {
    /// Construct a new [`ThreadPagination`].
    pub(in super::super::super) fn new(
        cache: Arc<RoomEventCacheInner>,
        thread_id: OwnedEventId,
    ) -> Self {
        Self(Pagination::new(ThreadEventCacheWrapper {
            cache,
            thread_id,
            dummy_pagination_status: SharedObservable::new(PaginationStatus::Idle {
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
    fn status(&self) -> &SharedObservable<PaginationStatus> {
        &self.dummy_pagination_status
    }

    async fn load_more_events_backwards(&self) -> Result<LoadMoreEventsBackwardsOutcome> {
        Ok(self
            .cache
            .state
            .write()
            .await?
            .get_or_reload_thread(self.thread_id.clone())
            .load_more_events_backwards())
    }

    async fn mark_has_waited_for_initial_prev_token(&self) -> Result<()> {
        *self.cache.state.write().await?.waited_for_initial_prev_token() = true;

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
            .relations(self.thread_id.clone(), options)
            .await
            .map_err(|err| EventCacheError::BackpaginationError(Box::new(err)))?;

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
                room.load_or_fetch_event(&self.thread_id, None)
                    .await
                    .map_err(|err| EventCacheError::BackpaginationError(Box::new(err)))?,
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

        if let Some(outcome) = state
            .get_or_reload_thread(self.thread_id.clone())
            .finish_network_pagination(prev_token, new_token, events)
        {
            Ok(Some(outcome))
        } else {
            // The previous token has gone missing, so the timeline has been reset in the
            // meanwhile, but it's fine per this function's contract.
            Ok(None)
        }
    }
}
