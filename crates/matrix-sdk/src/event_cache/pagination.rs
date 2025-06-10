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

//! A sub-object for running pagination tasks on a given room.

use std::{sync::Arc, time::Duration};

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk_base::timeout::timeout;
use matrix_sdk_common::linked_chunk::ChunkContent;
use ruma::api::Direction;
use tracing::{debug, instrument, trace};

use super::{
    room::{events::Gap, LoadMoreEventsBackwardsOutcome, RoomEventCacheInner},
    BackPaginationOutcome, EventsOrigin, Result, RoomEventCacheUpdate,
};
use crate::{event_cache::EventCacheError, room::MessagesOptions};

/// Status for the back-pagination on a room event cache.
#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum RoomPaginationStatus {
    /// No back-pagination is happening right now.
    Idle {
        /// Have we hit the start of the timeline, i.e. back-paginating wouldn't
        /// have any effect?
        hit_timeline_start: bool,
    },

    /// Back-pagination is already running in the background.
    Paginating,
}

/// Small RAII guard to reset the pagination status on drop, if not disarmed in
/// the meanwhile.
struct ResetStatusOnDrop {
    prev_status: Option<RoomPaginationStatus>,
    pagination_status: SharedObservable<RoomPaginationStatus>,
}

impl ResetStatusOnDrop {
    /// Make the RAII guard have no effect.
    fn disarm(mut self) {
        self.prev_status = None;
    }
}

impl Drop for ResetStatusOnDrop {
    fn drop(&mut self) {
        if let Some(status) = self.prev_status.take() {
            let _ = self.pagination_status.set(status);
        }
    }
}

/// An API object to run pagination queries on a [`super::RoomEventCache`].
///
/// Can be created with [`super::RoomEventCache::pagination()`].
#[allow(missing_debug_implementations)]
#[derive(Clone)]
pub struct RoomPagination {
    pub(super) inner: Arc<RoomEventCacheInner>,
}

impl RoomPagination {
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
    #[instrument(skip(self))]
    pub async fn run_backwards_until(
        &self,
        num_requested_events: u16,
    ) -> Result<BackPaginationOutcome> {
        let mut events = Vec::new();

        loop {
            if let Some(outcome) = self.run_backwards_impl(num_requested_events).await? {
                events.extend(outcome.events);
                if outcome.reached_start || events.len() >= num_requested_events as usize {
                    return Ok(BackPaginationOutcome {
                        reached_start: outcome.reached_start,
                        events,
                    });
                }
                trace!(
                    "restarting back-pagination, because we haven't reached \
                     the start or obtained enough events yet"
                );
            }

            debug!("restarting back-pagination because of a timeline reset.");
        }
    }

    /// Run a single back-pagination for the requested number of events.
    ///
    /// This automatically takes care of waiting for a pagination token from
    /// sync, if we haven't done that before.
    #[instrument(skip(self))]
    pub async fn run_backwards_once(&self, batch_size: u16) -> Result<BackPaginationOutcome> {
        loop {
            if let Some(outcome) = self.run_backwards_impl(batch_size).await? {
                return Ok(outcome);
            }
            debug!("restarting back-pagination because of a timeline reset.");
        }
    }

    /// Paginate from either the storage or the network, and let pagination
    /// status observers know about updates.
    async fn run_backwards_impl(&self, batch_size: u16) -> Result<Option<BackPaginationOutcome>> {
        // There is at least one gap that must be resolved; reach the network.
        // First, ensure there's no other ongoing back-pagination.
        let status_observable = &self.inner.pagination_status;

        let prev_status = status_observable.set(RoomPaginationStatus::Paginating);
        if !matches!(prev_status, RoomPaginationStatus::Idle { .. }) {
            return Err(EventCacheError::AlreadyBackpaginating);
        }

        let reset_status_on_drop_guard = ResetStatusOnDrop {
            prev_status: Some(prev_status),
            pagination_status: status_observable.clone(),
        };

        match self.paginate_backwards_impl(batch_size).await? {
            Some(outcome) => {
                // Back-pagination's over and successful, don't reset the status to the previous
                // value.
                reset_status_on_drop_guard.disarm();

                // Notify subscribers that pagination ended.
                status_observable
                    .set(RoomPaginationStatus::Idle { hit_timeline_start: outcome.reached_start });

                Ok(Some(outcome))
            }

            None => {
                // We keep the previous status value, because we haven't obtained more
                // information about the pagination.
                Ok(None)
            }
        }
    }

    /// Paginate from either the storage or the network.
    ///
    /// This method isn't concerned with setting the pagination status; only the
    /// caller is.
    async fn paginate_backwards_impl(
        &self,
        batch_size: u16,
    ) -> Result<Option<BackPaginationOutcome>> {
        // A linked chunk might not be entirely loaded (if it's been lazy-loaded). Try
        // to load from storage first, then from network if storage indicated
        // there's no previous events chunk to load.

        loop {
            let mut state_guard = self.inner.state.write().await;

            match state_guard.load_more_events_backwards().await? {
                LoadMoreEventsBackwardsOutcome::WaitForInitialPrevToken => {
                    const DEFAULT_WAIT_FOR_TOKEN_DURATION: Duration = Duration::from_secs(3);

                    // Release the state guard while waiting, to not deadlock the sync task.
                    drop(state_guard);

                    // Otherwise, wait for a notification that we received a previous-batch token.
                    trace!("waiting for a pagination token…");
                    let _ = timeout(
                        self.inner.pagination_batch_token_notifier.notified(),
                        DEFAULT_WAIT_FOR_TOKEN_DURATION,
                    )
                    .await;
                    trace!("done waiting");

                    self.inner.state.write().await.waited_for_initial_prev_token = true;

                    // Retry!
                    //
                    // Note: the next call to `load_more_events_backwards` can't return
                    // `WaitForInitialPrevToken` because we've just set to
                    // `waited_for_initial_prev_token`, so this is not an infinite loop.
                    //
                    // Note 2: not a recursive call, because recursive and async have a bad time
                    // together.
                    continue;
                }

                LoadMoreEventsBackwardsOutcome::Gap { prev_token } => {
                    // We have a gap, so resolve it with a network back-pagination.
                    drop(state_guard);
                    return self.paginate_backwards_with_network(batch_size, prev_token).await;
                }

                LoadMoreEventsBackwardsOutcome::StartOfTimeline => {
                    return Ok(Some(BackPaginationOutcome { reached_start: true, events: vec![] }));
                }

                LoadMoreEventsBackwardsOutcome::Events {
                    events,
                    timeline_event_diffs,
                    reached_start,
                } => {
                    if !timeline_event_diffs.is_empty() {
                        let _ =
                            self.inner.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                                diffs: timeline_event_diffs,
                                origin: EventsOrigin::Cache,
                            });
                    }

                    return Ok(Some(BackPaginationOutcome {
                        reached_start,
                        // This is a backwards pagination. `BackPaginationOutcome` expects events to
                        // be in “reverse order”.
                        events: events.into_iter().rev().collect(),
                    }));
                }
            }
        }
    }

    /// Run a single pagination request (/messages) to the server.
    ///
    /// If there are no previous-batch tokens, it will wait for one for a short
    /// while to get one, or if it's already done so or if it's seen a
    /// previous-batch token before, it will immediately indicate it's
    /// reached the end of the timeline.
    async fn paginate_backwards_with_network(
        &self,
        batch_size: u16,
        prev_token: Option<String>,
    ) -> Result<Option<BackPaginationOutcome>> {
        let (events, new_gap) = {
            let Some(room) = self.inner.weak_room.get() else {
                // The client is shutting down, return an empty default response.
                return Ok(Some(BackPaginationOutcome {
                    reached_start: false,
                    events: Default::default(),
                }));
            };

            let mut options = MessagesOptions::new(Direction::Backward).from(prev_token.as_deref());
            options.limit = batch_size.into();

            let response = room.messages(options).await.map_err(|err| {
                EventCacheError::BackpaginationError(
                    crate::event_cache::paginator::PaginatorError::SdkError(Box::new(err)),
                )
            })?;

            let new_gap = response.end.map(|prev_token| Gap { prev_token });

            (response.chunk, new_gap)
        };

        // Make sure the `RoomEvents` isn't updated while we are saving events from
        // backpagination.
        let mut state = self.inner.state.write().await;

        // Check that the previous token still exists; otherwise it's a sign that the
        // room's timeline has been cleared.
        let prev_gap_chunk_id = if let Some(token) = prev_token {
            let gap_chunk_id = state.events().chunk_identifier(|chunk| {
                matches!(chunk.content(), ChunkContent::Gap(Gap { ref prev_token }) if *prev_token == token)
            });

            if gap_chunk_id.is_none() {
                // We got a previous-batch token from the linked chunk *before* running the
                // request, but it is missing *after* completing the
                // request.
                //
                // It may be a sign the linked chunk has been reset, but it's fine, per this
                // function's contract.
                return Ok(None);
            }

            gap_chunk_id
        } else {
            None
        };

        let (outcome, timeline_event_diffs) =
            state.handle_backpagination(events, new_gap, prev_gap_chunk_id).await?;

        if !timeline_event_diffs.is_empty() {
            let _ = self.inner.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                diffs: timeline_event_diffs,
                origin: EventsOrigin::Pagination,
            });
        }

        Ok(Some(outcome))
    }

    /// Returns a subscriber to the pagination status used for the
    /// back-pagination integrated to the event cache.
    pub fn status(&self) -> Subscriber<RoomPaginationStatus> {
        self.inner.pagination_status.subscribe()
    }
}

/// Pagination token data, indicating in which state is the current pagination.
#[derive(Clone, Debug, PartialEq)]
pub enum PaginationToken {
    /// We never had a pagination token, so we'll start back-paginating from the
    /// end, or forward-paginating from the start.
    None,
    /// We paginated once before, and we received a prev/next batch token that
    /// we may reuse for the next query.
    HasMore(String),
    /// We've hit one end of the timeline (either the start or the actual end),
    /// so there's no need to continue paginating.
    HitEnd,
}

impl From<Option<String>> for PaginationToken {
    fn from(token: Option<String>) -> Self {
        match token {
            Some(val) => Self::HasMore(val),
            None => Self::None,
        }
    }
}
