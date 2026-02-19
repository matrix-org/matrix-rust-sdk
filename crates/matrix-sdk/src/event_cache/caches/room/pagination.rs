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
use matrix_sdk_base::event_cache::Event;
use ruma::api::Direction;

use super::super::super::{
    EventCacheError, EventsOrigin, Result, RoomEventCacheGenericUpdate, RoomEventCacheUpdate,
    TimelineVectorDiffs,
    caches::pagination::{
        BackPaginationOutcome, LoadMoreEventsBackwardsOutcome, PaginatedCache, Pagination,
    },
    room::RoomEventCacheInner,
};
pub use super::super::pagination::PaginationStatus;
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
            let _ = self.update_sender.send(RoomEventCacheUpdate::UpdateTimelineEvents(
                TimelineVectorDiffs { diffs: timeline_event_diffs, origin: EventsOrigin::Cache },
            ));

            // Send a room event cache generic update.
            let _ = self
                .generic_update_sender
                .send(RoomEventCacheGenericUpdate { room_id: self.room_id.clone() });
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
        new_token: Option<String>,
    ) -> Result<Option<BackPaginationOutcome>> {
        if let Some((outcome, timeline_event_diffs)) =
            self.state.write().await?.handle_backpagination(events, new_token, prev_token).await?
        {
            if !timeline_event_diffs.is_empty() {
                let _ = self.update_sender.send(RoomEventCacheUpdate::UpdateTimelineEvents(
                    TimelineVectorDiffs {
                        diffs: timeline_event_diffs,
                        origin: EventsOrigin::Pagination,
                    },
                ));

                // Send a room event cache generic update.
                let _ = self
                    .generic_update_sender
                    .send(RoomEventCacheGenericUpdate { room_id: self.room_id.clone() });
            }

            Ok(Some(outcome))
        } else {
            // The previous token has gone missing, so the timeline has been reset in the
            // meanwhile, but it's fine per this function's contract.
            Ok(None)
        }
    }
}
