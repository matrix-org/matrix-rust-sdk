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

//! The logic to paginate a cache (room, thread…) over the disk or the network.

use std::{pin::Pin, time::Duration};

use eyeball::SharedObservable;
use eyeball_im::VectorDiff;
use futures_util::{FutureExt as _, future::Shared};
use matrix_sdk_base::{SendOutsideWasm, SyncOutsideWasm, event_cache::Event, timeout::timeout};
use matrix_sdk_common::executor::{JoinHandle, spawn};
use tokio::sync::Mutex;
use tracing::{debug, instrument, trace, warn};

use super::super::Result;

/// Type to run paginations.
#[derive(Clone, Debug)]
pub(in super::super) struct Pagination<C: SendOutsideWasm + 'static> {
    pub cache: C,
}

impl<C: SendOutsideWasm + 'static> Pagination<C> {
    /// Create a new [`Pagination`].
    pub fn new(cache: C) -> Self {
        Self { cache }
    }
}

impl<C> Pagination<C>
where
    C: Clone + PaginatedCache + SendOutsideWasm + 'static + SyncOutsideWasm,
{
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

            debug!("restarting back-pagination because of a timeline reset");
        }
    }

    /// Paginate from either the storage or the network, and let pagination
    /// status observers know about updates.
    ///
    /// Returns `Ok(None)` if the pagination token used during a network
    /// pagination has disappeared from the in-memory linked chunk after
    /// handling the response.
    async fn run_backwards_impl(&self, batch_size: u16) -> Result<Option<BackPaginationOutcome>> {
        // There is at least one gap that must be resolved; reach the network.
        // First, ensure there's no other ongoing back-pagination.
        let status_observable = self.cache.status();

        let shared_pagination_lock = self.cache.shared_pagination();
        let mut shared_pagination_guard = shared_pagination_lock.lock().await;

        let prev_status = status_observable.set(PaginationStatus::Paginating);

        if !matches!(prev_status, PaginationStatus::Idle { .. })
            && let Some(shared_pagination) = shared_pagination_guard.as_ref()
        {
            // There was already a back-pagination request in progress; wait for it to
            // finish and return its result.
            return shared_pagination.wait_for_completion().await;
        }

        // If the previous status wasn't idle, but there was no shared future, we've hit
        // a race: the status might have been `Paginating`, but the pagination request
        // may have just finished thereafter. In that case, we can just start another
        // pagination.

        let reset_status_on_drop_guard = ResetStatusOnDrop {
            prev_status: Some(prev_status),
            pagination_status: status_observable.clone(),
        };

        let this = self.clone();

        let fut: Pin<Box<dyn SharedPaginationFuture>> = Box::pin(async move {
            match this.paginate_backwards_impl(batch_size).await? {
                Some(outcome) => {
                    // Back-pagination's over and successful, don't reset the status to the previous
                    // value.
                    reset_status_on_drop_guard.disarm();

                    // Notify subscribers that pagination ended.
                    this.cache
                        .status()
                        .set(PaginationStatus::Idle { hit_timeline_start: outcome.reached_start });

                    Ok(Some(outcome))
                }

                None => Ok(None),
            }
        });

        let shared_task = fut.shared();

        // Start polling in the background, in a spawned task.
        let shared_task_clone = shared_task.clone();
        let cloned_cache = self.cache.clone();
        let join_handle = spawn(async move {
            if let Err(err) = shared_task_clone.await {
                warn!("event cache back-pagination failed: {err}");
            }

            // Reset the pagination to `None`, independently of the result.
            *cloned_cache.shared_pagination().lock().await = None;
        });

        *shared_pagination_guard =
            Some(SharedPagination { fut: shared_task.clone(), _join_handle: join_handle });

        // Release the shared lock before waiting for the task to complete.
        drop(shared_pagination_guard);

        shared_task.await
    }

    /// Paginate from either the storage or the network.
    ///
    /// This method isn't concerned with setting the pagination status; only the
    /// caller is.
    ///
    /// Returns `Ok(None)` if the pagination token used during a network
    /// pagination has disappeared from the in-memory linked chunk after
    /// handling the response.
    async fn paginate_backwards_impl(
        &self,
        batch_size: u16,
    ) -> Result<Option<BackPaginationOutcome>> {
        // A linked chunk might not be entirely loaded (if it's been lazy-loaded). Try
        // to load from disk/storage first, then from network if disk/storage indicated
        // there's no previous events chunk to load.

        loop {
            match self.cache.load_more_events_backwards().await? {
                LoadMoreEventsBackwardsOutcome::Gap {
                    prev_token,
                    waited_for_initial_prev_token,
                } => {
                    if prev_token.is_none() && !waited_for_initial_prev_token {
                        // We didn't reload a pagination token, and we haven't waited for one; wait
                        // and start over.

                        const DEFAULT_WAIT_FOR_TOKEN_DURATION: Duration = Duration::from_secs(3);

                        // Otherwise, wait for a notification that we received a previous-batch
                        // token.
                        trace!("waiting for a pagination token…");

                        let _ = timeout(
                            self.cache.wait_for_prev_token(),
                            DEFAULT_WAIT_FOR_TOKEN_DURATION,
                        )
                        .await;

                        trace!("done waiting");

                        self.cache.mark_has_waited_for_initial_prev_token().await?;

                        // Retry!
                        //
                        // Note: the next call to `load_more_events_backwards` should not return
                        // `WaitForInitialPrevToken` because we've just marked we've waited for the
                        // initial `prev_token`, so this is not an infinite loop.
                        //
                        // Note 2: not a recursive call, because recursive and async have a bad time
                        // together.
                        continue;
                    }

                    // We have a gap, so resolve it with a network back-pagination.
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
                    return Ok(Some(
                        self.cache
                            .conclude_backwards_pagination_from_disk(
                                events,
                                timeline_event_diffs,
                                reached_start,
                            )
                            .await,
                    ));
                }
            }
        }
    }

    /// Run a single pagination request to the server.
    ///
    /// Returns `Ok(None)` if the pagination token used during the request has
    /// disappeared from the in-memory linked chunk after handling the
    /// response.
    async fn paginate_backwards_with_network(
        &self,
        batch_size: u16,
        prev_token: Option<String>,
    ) -> Result<Option<BackPaginationOutcome>> {
        let Some((events, new_token)) =
            self.cache.paginate_backwards_with_network(batch_size, &prev_token).await?
        else {
            // Return an empty default response.
            return Ok(Some(BackPaginationOutcome {
                reached_start: false,
                events: Default::default(),
            }));
        };

        self.cache.conclude_backwards_pagination_from_network(events, prev_token, new_token).await
    }
}

trait SharedPaginationFuture:
    Future<Output = Result<Option<BackPaginationOutcome>>> + SendOutsideWasm
{
}

impl<T: Future<Output = Result<Option<BackPaginationOutcome>>> + SendOutsideWasm>
    SharedPaginationFuture for T
{
}

/// State for having a pagination run in the background, and be awaited upon by
/// several tasks.
///
/// Such a pagination may be started automatically or manually. It's possible
/// for a manual caller to wait upon its completion, by awaiting the underlying
/// shared future.
pub(in super::super) struct SharedPagination {
    /// The shared future for a pagination request running in the background, so
    /// that multiple callers can await it.
    fut: Shared<Pin<Box<dyn SharedPaginationFuture>>>,

    /// The owned task that started the above future.
    _join_handle: JoinHandle<()>,
}

impl SharedPagination {
    /// Given that the shared request already exists, wait for it to complete,
    /// and return the same (cloned) result.
    async fn wait_for_completion(&self) -> Result<Option<BackPaginationOutcome>> {
        self.fut.clone().await
    }
}

pub(in super::super) trait PaginatedCache {
    fn status(&self) -> &SharedObservable<PaginationStatus>;
    fn shared_pagination(&self) -> &Mutex<Option<SharedPagination>>;

    fn load_more_events_backwards(
        &self,
    ) -> impl Future<Output = Result<LoadMoreEventsBackwardsOutcome>> + SendOutsideWasm;

    fn mark_has_waited_for_initial_prev_token(
        &self,
    ) -> impl Future<Output = Result<()>> + SendOutsideWasm;

    fn wait_for_prev_token(&self) -> impl Future<Output = ()> + SendOutsideWasm;

    fn paginate_backwards_with_network(
        &self,
        batch_size: u16,
        prev_token: &Option<String>,
    ) -> impl Future<Output = Result<Option<(Vec<Event>, Option<String>)>>> + SendOutsideWasm;

    fn conclude_backwards_pagination_from_disk(
        &self,
        events: Vec<Event>,
        timeline_event_diffs: Vec<VectorDiff<Event>>,
        reached_start: bool,
    ) -> impl Future<Output = BackPaginationOutcome> + SendOutsideWasm;

    fn conclude_backwards_pagination_from_network(
        &self,
        events: Vec<Event>,
        prev_token: Option<String>,
        new_token: Option<String>,
    ) -> impl Future<Output = Result<Option<BackPaginationOutcome>>> + SendOutsideWasm;
}

/// Status for the pagination on a cache.
#[derive(Debug, PartialEq, Clone, Copy)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum PaginationStatus {
    /// No pagination is happening right now.
    Idle {
        /// Have we hit the start of the timeline, i.e. paginating wouldn't
        /// have any effect?
        hit_timeline_start: bool,
    },

    /// Pagination is already running in the background.
    Paginating,
}

/// Small RAII guard to reset the pagination status on drop, if not disarmed in
/// the meanwhile.
struct ResetStatusOnDrop {
    prev_status: Option<PaginationStatus>,
    pagination_status: SharedObservable<PaginationStatus>,
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

/// The result of a single back-pagination request.
#[derive(Clone, Debug)]
pub struct BackPaginationOutcome {
    /// Did the back-pagination reach the start of the timeline?
    pub reached_start: bool,

    /// All the events that have been returned in the back-pagination
    /// request.
    ///
    /// Events are presented in reverse order: the first element of the vec,
    /// if present, is the most "recent" event from the chunk (or
    /// technically, the last one in the topological ordering).
    pub events: Vec<Event>,
}

/// Internal type to represent the output of
/// [`PaginatedCache::load_more_events_backwards`].
#[derive(Debug)]
pub(in super::super) enum LoadMoreEventsBackwardsOutcome {
    /// A gap has been inserted.
    Gap {
        /// The previous batch token to be used as the "end" parameter in the
        /// back-pagination request.
        prev_token: Option<String>,

        waited_for_initial_prev_token: bool,
    },

    /// The start of the timeline has been reached.
    StartOfTimeline,

    /// Events have been inserted.
    Events { events: Vec<Event>, timeline_event_diffs: Vec<VectorDiff<Event>>, reached_start: bool },
}
