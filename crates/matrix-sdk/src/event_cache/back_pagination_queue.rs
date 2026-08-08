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

//! A single component that owns one background task and executes
//! back-pagination requests coming from various use cases (search backfill, the
//! latest-event resolver, read-receipt finding) through one code path.
//!
//! Callers enqueue a [`BackPaginationRequest`] describing which room to
//! back-paginate, at which priority, and until when and they get a
//! [`BackPaginationHandle`] to await.
//!
//! Requests come in two shapes: rapid, shallow seeks for a room's most recent
//! visible events (latest-event resolution, read-receipt hunts, viewport
//! fills; small batches, stop predicates that fire almost immediately), and
//! slow bulk spidering of history (the search backfill; bigger batches, no
//! deadline).
//!
//! The executor:
//! - runs at most [`EventCacheConfig::max_concurrent_back_paginations`]
//!   requests at once, and additionally caps each priority class to its own
//!   concurrency ([`Priority::max_active`]) so sweeps over many rooms (latest
//!   events, read receipts) cannot monopolize the budget and flood the
//!   homeserver while a sync catch-up is in progress
//! - schedules by [`Priority`], higher first; within a priority, requests
//!   carrying a room recency stamp run most-recent first (so e.g. the
//!   latest-event backlog drains in reverse chronological order, keeping the
//!   room list accurate from the top down), and requests without one run FIFO
//!   after them
//! - never runs two requests for the same room concurrently, per-room
//!   single-flight
//! - attaches a request for a room already queued or running at the same
//!   priority onto that run as an extra [`Need`], rather than paginating the
//!   same history twice: one walk serves every need, each resolving
//!   individually as soon as its own stop condition or batch budget is met
//!   (e.g. a latest-event seek and a read-receipt hunt for the same room
//!   share a single walk).
//!
//! Requests are meant to be short, so a higher-priority request for a busy
//! room only waits for the current run, not a full sweep. Latest-event and
//! read-receipt requests rely on predicates that fire once a suitable
//! candidate is loaded, and every request kind additionally carries a batch
//! cap, so no request runs indefinitely.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
    ops::ControlFlow,
    sync::{Arc, Mutex as StdMutex, Weak},
    time::Duration,
};

use matrix_sdk_base::{RoomRecencyStamp, sleep::sleep, task_monitor::TaskMonitor};
use matrix_sdk_common::executor::spawn;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, RoomId};
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, info, instrument, trace, warn};

use super::{EventCacheInner, caches::pagination::BackPaginationOutcome};

/// A week, in milliseconds.
const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// How far back a search backfill goes: ~3 months, one week at a time.
const MAX_BACKFILL_WEEKS: u64 = 13;

/// How many rooms are enqueued and drained together before the next batch is
/// taken, within a week of the search backfill sweep.
const ROOM_BATCH: usize = 100;

/// Number of read-receipt paginations allowed per request.
///
/// The read-receipt hunt is only enqueued when the cached events resolve
/// nothing, and its stop predicate fires on the first event that proves the
/// room read or unread (see `stop_for_read_receipt_hunt`), so it's a shallow
/// seek exactly like the latest-event one, sharing its batch size. The budget
/// is slightly larger because the hunt starts where the cache ends, which in
/// pathological rooms is a run of uninteresting state churn. A room that
/// exhausts the budget is remembered and not hunted again until its receipt
/// changes (see [`BackPaginationQueue::paginate_for_read_receipt`]).
const READ_RECEIPT_MAX_BATCHES: usize = 3;

/// Number of paginations allowed per latest-event request.
///
/// One batch of [`LATEST_EVENT_BATCH_SIZE`] events is the whole budget: the
/// latest-event search is a shallow peek at the room's recent history, not a
/// deep scan. A room with no suitable candidate within that window keeps no
/// value until new activity arrives, rather than paginating its history away
/// looking for one.
const LATEST_EVENT_MAX_BATCHES: usize = 1;

/// Number of events requested per latest-event or read-receipt pagination.
///
/// Deliberately small (compared to [`BATCH_SIZE`]): these seeks only want the
/// most recent visible events, and searching more than a few events deep
/// rarely changes the outcome. The bulk use case (search backfill) keeps the
/// larger [`BATCH_SIZE`].
const LATEST_EVENT_BATCH_SIZE: u16 = 10;

/// Number of paginations allowed per viewport request.
///
/// Same rationale as [`LATEST_EVENT_MAX_BATCHES`]: the stop predicate usually
/// fires on the first batch, the cap bounds rooms whose recent history is
/// mostly non-displayable events.
const VIEWPORT_MAX_BATCHES: usize = 3;

/// Number of events requested per background pagination batch.
const BATCH_SIZE: u16 = 30;

/// How aggressively a search backfill runs.
#[derive(Clone, Copy, Debug)]
pub enum BackPaginationStrategy {
    /// The app is in the foreground: pause between paginations so this
    /// doesn't compete with interactive traffic.
    Foreground,
    /// A time-boxed background task (e.g. iOS `BGAppRefreshTask`) where there's
    /// no interactive traffic to protect.
    Background,
}

impl BackPaginationStrategy {
    /// How long to wait between introducing successive rooms into a search
    /// sweep (not between a single room's own pagination batches).
    fn enqueue_delay(self) -> Option<Duration> {
        match self {
            Self::Foreground => Some(Duration::from_secs(1)),
            Self::Background => None,
        }
    }
}

/// Priority of a [`BackPaginationRequest`], relative to the others in the
/// queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Priority {
    /// Bulk work with no deadline (search backfill).
    Low,
    /// Speculative work that saves a network round-trip later (timeline
    /// prefetch for viewport rooms): above the search sweep, below all
    /// reactive work.
    Prefetch,
    /// Reactive, user-facing work that wants a result promptly: the shallow
    /// seeks for a room's most recent visible event (latest-event resolution,
    /// read-receipt hunts). Sharing one priority means concurrent seeks for
    /// the same room coalesce into a single walk.
    High,
    /// The user is looking at the room right now (room-list viewport preview
    /// fill): beats everything else.
    Viewport,
}

impl Priority {
    /// How many requests of this priority may run concurrently, further
    /// bounded by the queue's overall concurrency.
    ///
    /// The latest-event sweep and the read-receipt hunts are background
    /// housekeeping over potentially thousands of rooms: after a cleared
    /// cache, letting them saturate the whole concurrency budget floods the
    /// homeserver with enough `/messages` traffic to slow the sync rounds the
    /// sweep feeds on (measured: room-list rounds at 8-13s under the flood,
    /// 2.5s without). Capping them leaves sync bandwidth untouched while the
    /// backlog still drains, most-recent room first.
    fn max_active(self) -> usize {
        match self {
            Self::High => 2,
            Self::Low | Self::Prefetch | Self::Viewport => usize::MAX,
        }
    }
}

/// A predicate over a freshly loaded batch, deciding whether to stop.
type BatchStopPredicate = Box<dyn FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send>;

/// When a single room's back-pagination run should stop.
enum StopCondition {
    /// Stop once a batch contains an event at or older than this timestamp.
    /// Used by the search backfill to fill history down to a time floor.
    OlderThan(MilliSecondsSinceUnixEpoch),

    /// Stop when a freshly loaded batch satisfies the predicate.
    /// Used by the latest-event resolver (a suitable candidate is loaded) and
    /// read-receipt finding (a target event id is loaded). A predicate that
    /// never fires leaves the run bounded only by `max_batches`, the start of
    /// the timeline, or cancellation.
    WhenBatch(BatchStopPredicate),
}

impl std::fmt::Debug for StopCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OlderThan(ts) => f.debug_tuple("OlderThan").field(ts).finish(),
            Self::WhenBatch(_) => f.write_str("WhenBatch(_)"),
        }
    }
}

/// Number of paginations allowed per room, per week, in a search backfill.
///
/// Bounds how long a single very active room can occupy a concurrency slot: a
/// room that doesn't reach the week's floor within this many batches is
/// retried on the next sweep, rather than blocking higher-priority work
/// indefinitely (there's no preemption, so requests must be self-limiting).
const SEARCH_MAX_BATCHES_PER_ROOM: usize = 10;

/// A request to back-paginate one room, enqueued on the
/// [`BackPaginationQueue`].
#[derive(Debug)]
struct BackPaginationRequest {
    /// The room to back-paginate.
    room_id: OwnedRoomId,
    /// Scheduling priority.
    priority: Priority,
    /// The room's recency stamp at enqueue time, if the caller wants requests
    /// of the same priority scheduled most-recent room first rather than FIFO.
    recency: Option<RoomRecencyStamp>,
    /// When to stop.
    stop: StopCondition,
    /// Number of events to request per pagination.
    batch_size: u16,
    /// Maximum number of paginations for this request (`None` = unbounded).
    max_batches: Option<usize>,
}

impl BackPaginationRequest {
    /// A search-backfill request: back-paginate down to `floor`, at [`Low`]
    /// priority, capped at a small number of batches (a very active room is
    /// retried on the next sweep rather than left to run indefinitely).
    fn search(room_id: OwnedRoomId, floor: MilliSecondsSinceUnixEpoch, batch_size: u16) -> Self {
        Self {
            room_id,
            priority: Priority::Low,
            recency: None,
            stop: StopCondition::OlderThan(floor),
            batch_size,
            max_batches: Some(SEARCH_MAX_BATCHES_PER_ROOM),
        }
    }

    /// A latest-event request: back-paginate until `stop` finds a candidate, at
    /// [`High`] priority, capped at [`LATEST_EVENT_MAX_BATCHES`] batches.
    ///
    /// `recency` is the room's recency stamp: the pending latest-event backlog
    /// is scheduled most-recent room first, so the room list fills in
    /// accurately from the top down.
    fn latest_event(
        room_id: OwnedRoomId,
        recency: Option<RoomRecencyStamp>,
        batch_size: u16,
        stop: impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static,
    ) -> Self {
        Self {
            room_id,
            priority: Priority::High,
            recency,
            stop: StopCondition::WhenBatch(Box::new(stop)),
            batch_size,
            max_batches: Some(LATEST_EVENT_MAX_BATCHES),
        }
    }

    /// A viewport request: the user is looking at the room in the room list;
    /// back-paginate until `stop` fires (enough displayable events are
    /// loaded), at [`Viewport`] priority, capped at [`VIEWPORT_MAX_BATCHES`]
    /// batches.
    fn viewport(
        room_id: OwnedRoomId,
        batch_size: u16,
        stop: impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static,
    ) -> Self {
        Self {
            room_id,
            priority: Priority::Viewport,
            recency: None,
            stop: StopCondition::WhenBatch(Box::new(stop)),
            batch_size,
            max_batches: Some(VIEWPORT_MAX_BATCHES),
        }
    }

    /// A prefetch request: load one batch of history at [`Prefetch`] priority,
    /// so that e.g. opening the room later needs no network round-trip.
    fn prefetch(room_id: OwnedRoomId, batch_size: u16) -> Self {
        Self {
            room_id,
            priority: Priority::Prefetch,
            recency: None,
            // One batch is the request: stop as soon as it has loaded.
            stop: StopCondition::WhenBatch(Box::new(|_| ControlFlow::Break(()))),
            batch_size,
            max_batches: Some(1),
        }
    }

    /// A read-receipt request: a shallow seek for the first event resolving
    /// the room's unread-ness, at [`High`] priority like the latest-event
    /// seek it closely mirrors (so the two coalesce when concurrent for the
    /// same room), capped at [`READ_RECEIPT_MAX_BATCHES`].
    ///
    /// Carries no recency stamp, so within [`High`] it schedules after the
    /// stamped latest-event backlog.
    fn read_receipt(
        room_id: OwnedRoomId,
        stop: impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static,
    ) -> Self {
        Self {
            room_id,
            priority: Priority::High,
            recency: None,
            stop: StopCondition::WhenBatch(Box::new(stop)),
            batch_size: LATEST_EVENT_BATCH_SIZE,
            max_batches: Some(READ_RECEIPT_MAX_BATCHES),
        }
    }
}

/// Why a single room's back-pagination run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RoomBackPaginationEnd {
    /// Reached the start of the room's timeline; nothing more to load.
    ReachedTimelineStart,
    /// The request's [`StopCondition`] was met.
    StopConditionMet,
    /// Hit `max_batches` without satisfying the stop condition or reaching the
    /// start of the timeline. More work likely remains; safe to retry later.
    BatchLimitReached,
    /// A pagination returned no events (e.g. a gap with no token to resolve it
    /// yet). Not an error; there's simply nothing to load right now.
    NoDataAvailable,
    /// Setting up or running the pagination failed.
    Failed,
    /// The request was cancelled.
    Cancelled,
}

/// The result of running a single [`BackPaginationRequest`] to completion.
#[derive(Clone, Copy, Debug)]
struct BackPaginationRunResult {
    /// Why the run ended.
    end: RoomBackPaginationEnd,
    /// The oldest event timestamp reached, if any events were loaded.
    reached: Option<MilliSecondsSinceUnixEpoch>,
}

/// A handle to an enqueued [`BackPaginationRequest`].
/// Dropping the handle cancels the request.
struct BackPaginationHandle {
    /// Cancels the request on drop; held only for its `Drop` side effect.
    _guard: DropGuard,
    completion: Option<oneshot::Receiver<BackPaginationRunResult>>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for BackPaginationHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackPaginationHandle").finish_non_exhaustive()
    }
}

impl BackPaginationHandle {
    /// Await the request's completion returning why it ended and the oldest
    /// event timestamp reached (if any events were loaded).
    async fn join(mut self) -> BackPaginationRunResult {
        let cancelled =
            BackPaginationRunResult { end: RoomBackPaginationEnd::Cancelled, reached: None };
        match self.completion.take() {
            Some(completion) => completion.await.unwrap_or(cancelled),
            None => cancelled,
        }
    }
}

/// A queue of background back-pagination requests, executed by priority with a
/// bounded in-flight number.
#[derive(Clone)]
pub struct BackPaginationQueue {
    inner: Arc<BackPaginationQueueInner>,
}

struct BackPaginationQueueInner {
    sender: mpsc::UnboundedSender<SubmittedRequest>,
    event_cache: Weak<EventCacheInner>,
    /// Per room, the read-receipt targets of the last hunt that exhausted its
    /// batch budget; identical hunts are skipped until the targets change.
    /// See [`BackPaginationQueue::paginate_for_read_receipt`].
    exhausted_receipt_hunts: StdMutex<HashMap<OwnedRoomId, HashSet<OwnedEventId>>>,
    _task: matrix_sdk_base::task_monitor::BackgroundTaskHandle,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for BackPaginationQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackPaginationQueue").finish_non_exhaustive()
    }
}

impl BackPaginationQueue {
    /// Create the queue and spawn its executor task.
    pub(super) fn new(
        event_cache: Weak<EventCacheInner>,
        max_concurrent: usize,
        task_monitor: &TaskMonitor,
    ) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let task = task_monitor.spawn_infinite_task(
            "event_cache::back_pagination_queue",
            scheduler(event_cache.clone(), receiver, max_concurrent),
        );

        Self {
            inner: Arc::new(BackPaginationQueueInner {
                sender,
                event_cache,
                exhausted_receipt_hunts: StdMutex::new(HashMap::new()),
                _task: task,
            }),
        }
    }

    /// Enqueue a new request returning a handle to await it.
    /// A request for a room already queued or running at the same priority is
    /// coalesced onto that run rather than starting a second one.
    fn enqueue(&self, request: BackPaginationRequest) -> BackPaginationHandle {
        let token = CancellationToken::new();
        let (completion_tx, completion_rx) = oneshot::channel();

        let submitted =
            SubmittedRequest { request, token: token.clone(), completion: completion_tx };

        if self.inner.sender.send(submitted).is_err() {
            // The executor is not available, resolve the handle as cancelled
            // straight away.
            token.cancel();
        }

        BackPaginationHandle { _guard: token.drop_guard(), completion: Some(completion_rx) }
    }

    /// Enqueue fire-and-forget and capped read-receipt back-pagination for a
    /// room. Used when the cached events don't resolve the room's unread-ness;
    /// `stop` fires once an event resolving it is loaded (see
    /// `stop_for_read_receipt_hunt`).
    ///
    /// `targets` are the receipt event ids the hunt is chasing, used to
    /// remember exhausted hunts: a hunt that hit its batch budget is not
    /// retried for the same targets (nothing has changed, it would fail
    /// identically, and in state-churn-heavy rooms each attempt downloads
    /// megabytes of useless history); the memo clears once the receipt (and
    /// so the target set) changes, or a later hunt succeeds.
    pub(crate) fn paginate_for_read_receipt(
        &self,
        room_id: &RoomId,
        targets: HashSet<OwnedEventId>,
        stop: impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static,
    ) {
        {
            let exhausted_hunts = self.inner.exhausted_receipt_hunts.lock().unwrap();
            if exhausted_hunts.get(room_id).is_some_and(|exhausted| *exhausted == targets) {
                debug!(
                    %room_id,
                    "skipping read-receipt backfill: an identical hunt already \
                     exhausted its batch budget"
                );
                return;
            }
        }

        let room_id = room_id.to_owned();
        debug!(%room_id, "started backfill request for read receipts");

        let handle = self.enqueue(BackPaginationRequest::read_receipt(room_id.clone(), stop));

        // Await completion in the background to record the outcome and log; the
        // spawned task itself is never awaited or aborted, so the request always
        // runs to completion regardless of this function's caller.
        let this = self.clone();
        spawn(async move {
            let BackPaginationRunResult { end, .. } = handle.join().await;

            {
                let mut exhausted_hunts = this.inner.exhausted_receipt_hunts.lock().unwrap();
                match end {
                    RoomBackPaginationEnd::BatchLimitReached => {
                        exhausted_hunts.insert(room_id.clone(), targets);
                    }
                    RoomBackPaginationEnd::StopConditionMet
                    | RoomBackPaginationEnd::ReachedTimelineStart => {
                        exhausted_hunts.remove(&room_id);
                    }
                    // Transient outcomes (failure, cancellation, no data):
                    // leave the memo alone, a retry may fare better.
                    _ => {}
                }
            }

            debug!(%room_id, ?end, "finished backfill request for read receipts");
        });
    }

    /// Back-paginate a room the user is currently looking at in the room-list
    /// viewport, until `stop` fires (enough displayable events are loaded) or
    /// the start of the timeline is reached. Runs before any other queued
    /// request.
    pub(crate) async fn paginate_for_viewport(
        &self,
        room_id: &RoomId,
        stop: impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static,
    ) -> RoomBackPaginationEnd {
        let room_id = room_id.to_owned();
        debug!(%room_id, "started backfill request for the viewport");

        let BackPaginationRunResult { end, .. } = self
            .enqueue(BackPaginationRequest::viewport(room_id.clone(), BATCH_SIZE, stop))
            .join()
            .await;

        debug!(%room_id, ?end, "finished backfill request for the viewport");

        end
    }

    /// Prefetch one batch of a room's history (e.g. so that opening it from
    /// the room list needs no network round-trip), at [`Prefetch`] priority:
    /// below all reactive work, above the search sweep.
    pub(crate) async fn paginate_for_prefetch(&self, room_id: &RoomId) -> RoomBackPaginationEnd {
        let room_id = room_id.to_owned();
        debug!(%room_id, "started prefetch request");

        let BackPaginationRunResult { end, .. } =
            self.enqueue(BackPaginationRequest::prefetch(room_id.clone(), BATCH_SIZE)).join().await;

        debug!(%room_id, ?end, "finished prefetch request");

        end
    }

    /// Back-paginate a room until `stop` finds a suitable latest-event
    /// candidate or the start of the timeline is reached.
    ///
    /// `recency` is the room's recency stamp: pending latest-event requests
    /// are scheduled most-recent room first (a request without a stamp runs
    /// after all stamped ones), so the room list fills in accurately from the
    /// top down.
    pub(crate) async fn paginate_for_latest_event(
        &self,
        room_id: &RoomId,
        recency: Option<RoomRecencyStamp>,
        stop: impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static,
    ) -> RoomBackPaginationEnd {
        let room_id = room_id.to_owned();
        debug!(%room_id, "started backfill request for latest events");

        let BackPaginationRunResult { end, .. } = self
            .enqueue(BackPaginationRequest::latest_event(
                room_id.clone(),
                recency,
                LATEST_EVENT_BATCH_SIZE,
                stop,
            ))
            .join()
            .await;

        debug!(%room_id, "finished backfill request for latest events");

        end
    }

    /// Sweep every room, back-paginating message history down to a
    /// MAX_BACKFILL_WEEKS floor to populate the search index (and the event
    /// cache).
    ///
    /// Coverage is front-loaded by recency: the last week is filled for all
    /// rooms first, then the previous week, and so on, in batches of rooms.
    /// Requests go on the shared queue at the lowest priority, so reactive work
    /// (latest event, read receipts) always takes precedence.
    ///
    /// `strategy` paces how fast new rooms are introduced into the sweep:
    /// [`BackPaginationStrategy::Foreground`] spaces them out so this doesn't
    /// compete with interactive traffic.
    /// [`BackPaginationStrategy::Background`] introduces them as fast as the
    /// concurrency cap allows.
    pub async fn run_search_backfill(&self, strategy: BackPaginationStrategy) {
        let enqueue_delay = strategy.enqueue_delay();
        let now = now_ms();
        let started = now;

        let total_rooms = self.rooms_by_relevancy().len();
        let target_floor = MilliSecondsSinceUnixEpoch(
            now.saturating_sub(MAX_BACKFILL_WEEKS * WEEK_MS).try_into().unwrap_or_default(),
        );
        info!(
            ?strategy,
            total_rooms,
            weeks = MAX_BACKFILL_WEEKS,
            target_date = format_date(target_floor),
            "search backfill started"
        );

        // Rooms that reached the start of their timeline.
        let mut drained: HashSet<OwnedRoomId> = HashSet::new();
        let mut floor = target_floor;

        for week in 1..=MAX_BACKFILL_WEEKS {
            floor = MilliSecondsSinceUnixEpoch(
                now.saturating_sub(week * WEEK_MS).try_into().unwrap_or_default(),
            );

            let rooms = self.rooms_by_relevancy();
            let rooms_to_process = rooms.iter().filter(|r| !drained.contains(*r)).count();
            debug!(
                week,
                of = MAX_BACKFILL_WEEKS,
                date = format_date(floor),
                rooms_to_process,
                "search backfill week"
            );

            for chunk in rooms.chunks(ROOM_BATCH) {
                // Enqueue the batch then wait for it to drain before moving to
                // the next batch / deeper week. The queue bounds actual
                // concurrency.
                let mut handles = Vec::new();
                for room_id in chunk.iter().filter(|room_id| !drained.contains(*room_id)) {
                    if !handles.is_empty()
                        && let Some(delay) = enqueue_delay
                    {
                        sleep(delay).await;
                    }
                    debug!(%room_id, "started search backfill");
                    handles.push((
                        room_id.clone(),
                        self.enqueue(BackPaginationRequest::search(
                            room_id.clone(),
                            floor,
                            BATCH_SIZE,
                        )),
                    ));
                }

                for (room_id, handle) in handles {
                    let BackPaginationRunResult { end, reached } = handle.join().await;
                    trace!(
                        %room_id,
                        week,
                        of = MAX_BACKFILL_WEEKS,
                        reached_date = reached.map(format_date),
                        "finished search backpagination request"
                    );
                    if end == RoomBackPaginationEnd::ReachedTimelineStart {
                        drained.insert(room_id);
                    }
                }
            }
        }

        info!(
            total_rooms,
            rooms_drained = drained.len(),
            reached_date = format_date(floor),
            elapsed_ms = now_ms().saturating_sub(started),
            "search backfill finished"
        );
    }

    /// Joined room ids, most-recently-active first.
    fn rooms_by_relevancy(&self) -> Vec<OwnedRoomId> {
        let Some(inner) = self.inner.event_cache.upgrade() else {
            return Vec::new();
        };
        let Some(client) = inner.client.get() else {
            return Vec::new();
        };

        let mut rooms = client.joined_rooms();
        rooms.sort_by_key(|room| std::cmp::Reverse(room.recency_stamp()));
        rooms.into_iter().map(|room| room.room_id().to_owned()).collect()
    }
}

/// The current time, in milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    u64::from(MilliSecondsSinceUnixEpoch::now().get())
}

/// Format a timestamp as a calendar date (`YYYY-MM-DD`), for logging.
fn format_date(ts: MilliSecondsSinceUnixEpoch) -> String {
    chrono::DateTime::from_timestamp_millis(i64::from(ts.get()))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".to_owned())
}

/// Identifies a run i.e. a room back-paginated at a given priority.
type RunKey = (OwnedRoomId, Priority);

/// One caller's participation in a run: its own stop condition, batch budget
/// and completion channel.
///
/// A run walks a room's history once, serving every attached need: each need
/// resolves individually as soon as its stop condition fires or its batch
/// budget runs out, while the run keeps walking for the others. This is what
/// makes e.g. a latest-event seek and a read-receipt hunt for the same room
/// one walk instead of two.
struct Need {
    /// When this need is satisfied.
    stop: StopCondition,
    /// Batches this need may still observe (`None` = unbounded). Counted from
    /// the batch after it attaches: a need attached mid-run only evaluates
    /// batches loaded after it (earlier ones are in the cache, which the
    /// caller consulted before enqueueing).
    batches_remaining: Option<usize>,
    /// Cancels just this need, not the run.
    token: CancellationToken,
    /// Where to report this need's outcome.
    completion: oneshot::Sender<BackPaginationRunResult>,
}

impl Need {
    /// Resolve this need, reporting why and how far the run got.
    fn complete(self, end: RoomBackPaginationEnd, reached: Option<MilliSecondsSinceUnixEpoch>) {
        let _ = self.completion.send(BackPaginationRunResult { end, reached });
    }
}

/// A request as it arrives on the queue's channel, before the scheduler has
/// turned it into a [`Need`] attached to a new or existing run.
struct SubmittedRequest {
    request: BackPaginationRequest,
    token: CancellationToken,
    completion: oneshot::Sender<BackPaginationRunResult>,
}

/// The needs waiting for a queued (not yet started) run.
struct PendingRun {
    /// Events per pagination; set by the run's first need (request kinds
    /// sharing a priority share a batch size).
    batch_size: u16,
    needs: Vec<Need>,
}

/// A queued run in the scheduler's heap: ordering information only, the needs
/// live in the scheduler's pending map (a heap entry can't be mutated in
/// place when a later need attaches). An entry whose key has no pending needs
/// (already started via an earlier entry) is stale, and skipped when popped.
struct ScheduledRun {
    room_id: OwnedRoomId,
    priority: Priority,
    /// The room's recency stamp at enqueue time of the run's FIRST need;
    /// later attachments don't reorder the queue.
    recency: Option<RoomRecencyStamp>,
    /// Insertion order, assigned by the scheduler, for FIFO within a priority.
    seq: u64,
}

impl ScheduledRun {
    fn key(&self) -> RunKey {
        (self.room_id.clone(), self.priority)
    }
}

impl PartialEq for ScheduledRun {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}

impl Eq for ScheduledRun {}

impl PartialOrd for ScheduledRun {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledRun {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first; within a priority, higher recency stamp first
        // (`None < Some(_)`, so requests without a stamp sort after every
        // request with one); then earlier `seq` first.
        self.priority
            .cmp(&other.priority)
            .then_with(|| self.recency.cmp(&other.recency))
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// The executor schedules runs by priority, bounded concurrency at one run
/// per room at a time; a request for a room already queued or running at the
/// same priority attaches to that run as an extra [`Need`] rather than
/// starting a second walk.
#[instrument(skip_all)]
async fn scheduler(
    event_cache: Weak<EventCacheInner>,
    mut receiver: mpsc::UnboundedReceiver<SubmittedRequest>,
    max_concurrent: usize,
) {
    trace!("Spawning the back-pagination queue executor");

    let mut scheduled_runs: BinaryHeap<ScheduledRun> = BinaryHeap::new();
    let mut pending_runs: HashMap<RunKey, PendingRun> = HashMap::new();
    // Active runs, by key; the sender attaches new needs to the running walk.
    let mut active_runs: HashMap<RunKey, mpsc::UnboundedSender<Need>> = HashMap::new();
    let mut next_seq: u64 = 0;

    // The executor is notified which run finished so it can free the room.
    // (Needs are completed by the run itself.)
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<RunKey>();

    loop {
        // Schedule as many pending runs as the concurrency budget and the
        // per-room single-flight rule allow.
        let mut active_keys: Vec<RunKey> = active_runs.keys().cloned().collect();
        for (key, run) in
            next_runnable(&mut scheduled_runs, &mut pending_runs, &mut active_keys, max_concurrent)
        {
            let PendingRun { batch_size, needs } = run;

            // Drop needs cancelled while queued.
            let needs: Vec<_> = needs
                .into_iter()
                .filter_map(|need| {
                    if need.token.is_cancelled() {
                        need.complete(RoomBackPaginationEnd::Cancelled, None);
                        None
                    } else {
                        Some(need)
                    }
                })
                .collect();

            if needs.is_empty() {
                continue;
            }

            trace!(
                room_id = %key.0,
                priority = ?key.1,
                needs = needs.len(),
                queued = scheduled_runs.len(),
                "back-pagination run scheduled"
            );

            let (needs_tx, needs_rx) = mpsc::unbounded_channel();
            active_runs.insert(key.clone(), needs_tx);

            let event_cache = event_cache.clone();
            let done_tx = done_tx.clone();
            spawn(async move {
                run_request(&event_cache, &key.0, key.1, batch_size, needs, needs_rx).await;
                let _ = done_tx.send(key);
            });
        }

        select! {
            request = receiver.recv() => {
                match request {
                    Some(SubmittedRequest { request, token, completion }) => {
                        attach_or_queue(
                            request,
                            token,
                            completion,
                            &active_runs,
                            &mut pending_runs,
                            &mut scheduled_runs,
                            &mut next_seq,
                        );
                    }
                    None => {
                        info!("Back-pagination queue sender closed, exiting");
                        break;
                    }
                }
            }

            Some(key) = done_rx.recv() => {
                active_runs.remove(&key);
            }
        }
    }
}

/// Turn a submitted request into a [`Need`] and route it: attach it to the
/// room's active run at the same priority if one is walking (it evaluates
/// from the next batch), else onto the queued run for that key, else open a
/// new queued run.
fn attach_or_queue(
    request: BackPaginationRequest,
    token: CancellationToken,
    completion: oneshot::Sender<BackPaginationRunResult>,
    active_runs: &HashMap<RunKey, mpsc::UnboundedSender<Need>>,
    pending_runs: &mut HashMap<RunKey, PendingRun>,
    scheduled_runs: &mut BinaryHeap<ScheduledRun>,
    next_seq: &mut u64,
) {
    let BackPaginationRequest { room_id, priority, recency, stop, batch_size, max_batches } =
        request;

    let mut need = Need { stop, batches_remaining: max_batches, token, completion };
    let key = (room_id, priority);

    if let Some(active) = active_runs.get(&key) {
        match active.send(need) {
            Ok(()) => {
                trace!(
                    room_id = %key.0,
                    priority = ?key.1,
                    "attached the need to the room's active run"
                );
                return;
            }
            // The run ended in the meantime; queue a new one instead.
            Err(mpsc::error::SendError(returned)) => need = returned,
        }
    }

    match pending_runs.entry(key) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            trace!(
                room_id = %entry.key().0,
                priority = ?entry.key().1,
                "attached the need to the room's queued run"
            );
            entry.get_mut().needs.push(need);
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            scheduled_runs.push(ScheduledRun {
                room_id: entry.key().0.clone(),
                priority: entry.key().1,
                recency,
                seq: *next_seq,
            });
            *next_seq += 1;
            entry.insert(PendingRun { batch_size, needs: vec![need] });
        }
    }
}

/// Pick the runs that can start right now, highest priority first: bounded by
/// `max_concurrent` total in flight, and never two runs for the same room.
///
/// Picked keys are appended to `active_keys`; runs popped but not yet
/// runnable (their room is busy, or their priority class is at its cap) are
/// pushed back onto the heap. Stale heap entries (whose needs were already
/// taken by an earlier entry for the same key) are dropped.
fn next_runnable(
    scheduled_runs: &mut BinaryHeap<ScheduledRun>,
    pending_runs: &mut HashMap<RunKey, PendingRun>,
    active_keys: &mut Vec<RunKey>,
    max_concurrent: usize,
) -> Vec<(RunKey, PendingRun)> {
    let mut picked = Vec::new();
    let mut skipped = Vec::new();

    while active_keys.len() < max_concurrent {
        let Some(entry) = scheduled_runs.pop() else {
            break;
        };

        let key = entry.key();

        if !pending_runs.contains_key(&key) {
            // Stale entry, drop it.
            continue;
        }

        if active_keys.iter().any(|(id, _)| *id == entry.room_id) {
            // This room is busy, try it again next round.
            skipped.push(entry);
            continue;
        }

        if active_keys.iter().filter(|(_, p)| *p == entry.priority).count()
            >= entry.priority.max_active()
        {
            // This priority class is at its own concurrency cap, try it again
            // next round.
            skipped.push(entry);
            continue;
        }

        let run = pending_runs.remove(&key).expect("checked above");
        active_keys.push(key.clone());
        picked.push((key, run));
    }

    for entry in skipped {
        scheduled_runs.push(entry);
    }

    picked
}

/// End a run: refuse further attachments, then resolve every remaining need
/// (including last-instant arrivals) with `end`.
fn finish_run(
    mut needs: Vec<Need>,
    incoming: &mut mpsc::UnboundedReceiver<Need>,
    end: RoomBackPaginationEnd,
    reached: Option<MilliSecondsSinceUnixEpoch>,
) {
    incoming.close();
    while let Ok(need) = incoming.try_recv() {
        needs.push(need);
    }
    for need in needs {
        need.complete(end, reached);
    }
}

/// Walk one room's history backwards, serving every attached need until each
/// has resolved (its [`StopCondition`] fired, or its batch budget ran out, or
/// its caller cancelled), or the walk itself ends (start of the timeline, no
/// data, failure).
///
/// New needs may attach mid-run via `incoming`; they evaluate batches loaded
/// from that point on.
#[instrument(skip_all, fields(room_id = %room_id, priority = ?priority))]
async fn run_request(
    event_cache: &Weak<EventCacheInner>,
    room_id: &RoomId,
    priority: Priority,
    batch_size: u16,
    mut needs: Vec<Need>,
    mut incoming: mpsc::UnboundedReceiver<Need>,
) {
    // Grab an owned `RoomPagination`, dropping the caches guard immediately so
    // we don't hold the room lock across network paginations.
    let pagination = {
        let Some(inner) = event_cache.upgrade() else {
            finish_run(needs, &mut incoming, RoomBackPaginationEnd::Cancelled, None);
            return;
        };
        match inner.all_caches_for_room(room_id).await {
            Ok(caches) => caches.room.pagination(),
            Err(err) => {
                warn!("no caches for room while back-paginating: {err}");
                finish_run(needs, &mut incoming, RoomBackPaginationEnd::Failed, None);
                return;
            }
        }
    };

    let mut oldest_reached: Option<MilliSecondsSinceUnixEpoch> = None;

    loop {
        // Absorb needs that attached while the previous batch was in flight.
        while let Ok(need) = incoming.try_recv() {
            needs.push(need);
        }

        // Resolve needs cancelled by their caller; the walk continues for the
        // others.
        needs = needs
            .into_iter()
            .filter_map(|need| {
                if need.token.is_cancelled() {
                    need.complete(RoomBackPaginationEnd::Cancelled, oldest_reached);
                    None
                } else {
                    Some(need)
                }
            })
            .collect();

        if needs.is_empty() {
            // Nobody is waiting any more: end the run. Refuse further
            // attachments first, and keep walking if one raced in (its
            // enqueuer saw this run as active, so nobody else will serve it).
            incoming.close();
            while let Ok(need) = incoming.try_recv() {
                needs.push(need);
            }
            if needs.is_empty() {
                return;
            }
        }

        let outcome = match pagination.run_backwards_once(batch_size).await {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!("back-pagination failed: {err}");
                finish_run(needs, &mut incoming, RoomBackPaginationEnd::Failed, oldest_reached);
                return;
            }
        };

        if let Some(batch_oldest) = oldest_event_timestamp(&outcome) {
            oldest_reached = Some(oldest_reached.map_or(batch_oldest, |cur| cur.min(batch_oldest)));
        }

        // Absorb needs that attached while this batch was in flight: they
        // haven't seen it, so they evaluate it below rather than waiting for
        // (and possibly triggering) another one.
        while let Ok(need) = incoming.try_recv() {
            needs.push(need);
        }

        if outcome.reached_start {
            finish_run(
                needs,
                &mut incoming,
                RoomBackPaginationEnd::ReachedTimelineStart,
                oldest_reached,
            );
            return;
        }

        // Resolve the needs whose stop condition fires on this batch.
        needs = needs
            .into_iter()
            .filter_map(|mut need| {
                if stop_now(&mut need.stop, &outcome) {
                    need.complete(RoomBackPaginationEnd::StopConditionMet, oldest_reached);
                    None
                } else {
                    Some(need)
                }
            })
            .collect();

        if outcome.events.is_empty() {
            finish_run(
                needs,
                &mut incoming,
                RoomBackPaginationEnd::NoDataAvailable,
                oldest_reached,
            );
            return;
        }

        // Spend one batch of every remaining need's budget.
        needs = needs
            .into_iter()
            .filter_map(|mut need| match &mut need.batches_remaining {
                Some(remaining) => {
                    *remaining -= 1;
                    if *remaining == 0 {
                        need.complete(RoomBackPaginationEnd::BatchLimitReached, oldest_reached);
                        None
                    } else {
                        Some(need)
                    }
                }
                None => Some(need),
            })
            .collect();
    }
}

/// The oldest event timestamp in a batch, if any.
fn oldest_event_timestamp(outcome: &BackPaginationOutcome) -> Option<MilliSecondsSinceUnixEpoch> {
    outcome.events.iter().filter_map(|event| event.timestamp()).min()
}

/// Evaluate a [`StopCondition`] against a freshly loaded batch.
fn stop_now(stop: &mut StopCondition, outcome: &BackPaginationOutcome) -> bool {
    match stop {
        StopCondition::OlderThan(floor) => {
            oldest_event_timestamp(outcome).is_some_and(|oldest| oldest <= *floor)
        }
        StopCondition::WhenBatch(predicate) => predicate(outcome).is_break(),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::ops::ControlFlow;

    use assert_matches::assert_matches;
    use eyeball_im::VectorDiff;
    use matrix_sdk_test::{BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{MilliSecondsSinceUnixEpoch, event_id, room_id};

    use super::{
        BackPaginationOutcome, BackPaginationRequest, BackPaginationStrategy, Need, PendingRun,
        Priority, RunKey, ScheduledRun, StopCondition, attach_or_queue, next_runnable, stop_now,
    };
    use crate::{
        assert_let_timeout,
        event_cache::{EventsOrigin, RoomEventCacheUpdate},
        test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    };

    /// A need whose stop condition never fires; scheduling tests never
    /// evaluate it.
    fn dummy_need() -> Need {
        Need {
            stop: StopCondition::WhenBatch(Box::new(|_| ControlFlow::Continue(()))),
            batches_remaining: None,
            token: tokio_util::sync::CancellationToken::new(),
            completion: tokio::sync::oneshot::channel().0,
        }
    }

    /// Build a queued run for a room, at a priority, with an insertion seq,
    /// registering its pending needs in `pending_runs`.
    fn queued(
        pending_runs: &mut std::collections::HashMap<RunKey, PendingRun>,
        room_id: ruma::OwnedRoomId,
        priority: Priority,
        seq: u64,
    ) -> ScheduledRun {
        queued_with_recency(pending_runs, room_id, priority, None, seq)
    }

    /// Build a queued run for a room, at a priority, with a recency stamp and
    /// an insertion seq, registering its pending needs in `pending_runs`.
    fn queued_with_recency(
        pending_runs: &mut std::collections::HashMap<RunKey, PendingRun>,
        room_id: ruma::OwnedRoomId,
        priority: Priority,
        recency: Option<super::RoomRecencyStamp>,
        seq: u64,
    ) -> ScheduledRun {
        pending_runs
            .entry((room_id.clone(), priority))
            .or_insert_with(|| PendingRun { batch_size: 10, needs: Vec::new() })
            .needs
            .push(dummy_need());
        ScheduledRun { room_id, priority, recency, seq }
    }

    /// `next_runnable` serves highest priority first, then FIFO within a
    /// priority.
    #[test]
    fn test_scheduling_priority_and_fifo() {
        use std::collections::BinaryHeap;

        let (a, b, c, d) = (room_id!("!a:e"), room_id!("!b:e"), room_id!("!c:e"), room_id!("!d:e"));

        let mut scheduled_requests = BinaryHeap::new();
        let mut pending_runs = std::collections::HashMap::new();
        // Push out of priority order, with monotonic seqs.
        scheduled_requests.push(queued(&mut pending_runs, a.to_owned(), Priority::Low, 0));
        scheduled_requests.push(queued(&mut pending_runs, b.to_owned(), Priority::High, 1));
        scheduled_requests.push(queued(&mut pending_runs, c.to_owned(), Priority::Prefetch, 2));
        scheduled_requests.push(queued(&mut pending_runs, d.to_owned(), Priority::High, 3));

        let mut active_requests = Vec::new();
        let picked: Vec<_> =
            next_runnable(&mut scheduled_requests, &mut pending_runs, &mut active_requests, 10)
                .into_iter()
                .map(|(key, _)| key.0)
                .collect();

        // High first (b before d by FIFO), then Prefetch, then Low.
        assert_eq!(picked, vec![b.to_owned(), d.to_owned(), c.to_owned(), a.to_owned()]);
    }

    /// Within a priority, requests carrying a recency stamp run most-recent
    /// room first regardless of enqueue order, and requests without one run
    /// FIFO after all stamped ones.
    #[test]
    fn test_scheduling_recency_within_priority() {
        use std::collections::BinaryHeap;

        let (a, b, c, d, e) = (
            room_id!("!a:e"),
            room_id!("!b:e"),
            room_id!("!c:e"),
            room_id!("!d:e"),
            room_id!("!e:e"),
        );

        let mut scheduled_requests = BinaryHeap::new();
        let mut pending_runs = std::collections::HashMap::new();
        // Enqueue stamped requests out of recency order, interleaved with
        // unstamped ones, all at the same priority.
        scheduled_requests.push(queued_with_recency(
            &mut pending_runs,
            a.to_owned(),
            Priority::High,
            None,
            0,
        ));
        scheduled_requests.push(queued_with_recency(
            &mut pending_runs,
            b.to_owned(),
            Priority::High,
            Some(10.into()),
            1,
        ));
        scheduled_requests.push(queued_with_recency(
            &mut pending_runs,
            c.to_owned(),
            Priority::High,
            Some(30.into()),
            2,
        ));
        scheduled_requests.push(queued_with_recency(
            &mut pending_runs,
            d.to_owned(),
            Priority::High,
            None,
            3,
        ));
        scheduled_requests.push(queued_with_recency(
            &mut pending_runs,
            e.to_owned(),
            Priority::High,
            Some(20.into()),
            4,
        ));
        // A higher priority still beats the highest recency stamp.
        let f = room_id!("!f:e");
        scheduled_requests.push(queued_with_recency(
            &mut pending_runs,
            f.to_owned(),
            Priority::Viewport,
            None,
            5,
        ));

        // Drain in waves: High is capped at 2 concurrent, so completions are
        // simulated by clearing the active list between calls. The cumulative
        // order proves the scheduling order.
        let mut picked = Vec::new();
        while !scheduled_requests.is_empty() {
            let mut active_requests = Vec::new();
            let wave =
                next_runnable(&mut scheduled_requests, &mut pending_runs, &mut active_requests, 10);
            assert!(!wave.is_empty(), "the scheduler must make progress");
            picked.extend(wave.into_iter().map(|(key, _)| key.0));
        }

        // Viewport first; then the stamped High requests by recency
        // descending (c, e, b); then the unstamped ones FIFO (a, d).
        assert_eq!(
            picked,
            vec![
                f.to_owned(),
                c.to_owned(),
                e.to_owned(),
                b.to_owned(),
                a.to_owned(),
                d.to_owned()
            ]
        );
    }

    /// A priority class never runs more than its own concurrency cap
    /// ([`Priority::max_active`]), even when the global budget has room:
    /// the recent-history seeks (High, 2: latest events and read receipts)
    /// must not flood the homeserver during a sync catch-up. Lower-priority
    /// classes behind a capped class still get scheduled.
    #[test]
    fn test_scheduling_per_priority_concurrency_cap() {
        use std::collections::BinaryHeap;

        let mut scheduled_requests = BinaryHeap::new();
        let mut pending_runs = std::collections::HashMap::new();
        for (i, room) in ["!a:e", "!b:e", "!c:e", "!d:e", "!e:e"].iter().enumerate() {
            scheduled_requests.push(queued(
                &mut pending_runs,
                ruma::RoomId::parse(room).unwrap(),
                Priority::High,
                i as u64,
            ));
        }
        scheduled_requests.push(queued(
            &mut pending_runs,
            ruma::RoomId::parse("!h:e").unwrap(),
            Priority::Low,
            7,
        ));

        let mut active_requests = Vec::new();
        let picked =
            next_runnable(&mut scheduled_requests, &mut pending_runs, &mut active_requests, 10);

        // 2 High + the Low one; the other 3 High wait.
        let mut priorities: Vec<_> = picked.iter().map(|(key, _)| key.1).collect();
        priorities.sort();
        assert_eq!(priorities, vec![Priority::Low, Priority::High, Priority::High]);
        assert_eq!(scheduled_requests.len(), 3);
    }

    /// `next_runnable` never returns more than `max_concurrent`.
    #[test]
    fn test_scheduling_respects_concurrency_cap() {
        use std::collections::BinaryHeap;

        let mut scheduled_requests = BinaryHeap::new();
        let mut pending_runs = std::collections::HashMap::new();
        for (i, room) in [room_id!("!a:e"), room_id!("!b:e"), room_id!("!c:e")].iter().enumerate() {
            // `Low` has no per-priority cap: only the global budget binds.
            scheduled_requests.push(queued(
                &mut pending_runs,
                (*room).to_owned(),
                Priority::Low,
                i as u64,
            ));
        }

        let mut active_requests = Vec::new();
        let picked =
            next_runnable(&mut scheduled_requests, &mut pending_runs, &mut active_requests, 2);

        assert_eq!(picked.len(), 2);
        assert_eq!(active_requests.len(), 2);
        // The third request stays queued.
        assert_eq!(scheduled_requests.len(), 1);
    }

    /// `next_runnable` won't start a room that's already active, nor two runs
    /// for the same room in one pass.
    #[test]
    fn test_scheduling_per_room_single_flight() {
        use std::collections::BinaryHeap;

        let (a, b) = (room_id!("!a:e"), room_id!("!b:e"));

        // `a` is already running.
        let mut active_requests = vec![(a.to_owned(), Priority::High)];

        let mut scheduled_requests = BinaryHeap::new();
        let mut pending_runs = std::collections::HashMap::new();
        // Same room as active, at another priority (same-priority requests
        // would have attached to the active run instead of queueing).
        scheduled_requests.push(queued(&mut pending_runs, a.to_owned(), Priority::Viewport, 0));
        scheduled_requests.push(queued(&mut pending_runs, b.to_owned(), Priority::Low, 2)); // a different room

        let picked: Vec<_> =
            next_runnable(&mut scheduled_requests, &mut pending_runs, &mut active_requests, 10)
                .into_iter()
                .map(|(key, _)| key.0)
                .collect();

        // Only `b` runs; the `a` run stays queued (a is busy).
        assert_eq!(picked, vec![b.to_owned()]);
        assert_eq!(scheduled_requests.len(), 1);
    }

    /// The first request for a room + priority opens a queued run; a later
    /// one at the same key attaches to it as an extra need; a different
    /// priority for the same room opens its own run; and while a run is
    /// active, a new request attaches to the running walk.
    #[test]
    fn test_attach_or_queue() {
        use std::collections::{BinaryHeap, HashMap};

        let a = room_id!("!a:e");

        let request = |priority| BackPaginationRequest {
            room_id: a.to_owned(),
            priority,
            recency: None,
            stop: StopCondition::WhenBatch(Box::new(|_| ControlFlow::Continue(()))),
            batch_size: 10,
            max_batches: None,
        };
        let token = tokio_util::sync::CancellationToken::new;
        let completion = || tokio::sync::oneshot::channel().0;

        let mut active = HashMap::new();
        let mut pending = HashMap::new();
        let mut heap = BinaryHeap::new();
        let mut seq = 0u64;

        let high = (a.to_owned(), Priority::High);
        let viewport = (a.to_owned(), Priority::Viewport);
        let low = (a.to_owned(), Priority::Low);

        // First request at (a, High): opens a queued run.
        attach_or_queue(
            request(Priority::High),
            token(),
            completion(),
            &active,
            &mut pending,
            &mut heap,
            &mut seq,
        );
        assert_eq!(heap.len(), 1);
        assert_eq!(pending[&high].needs.len(), 1);

        // Second request at the same key: attaches to the queued run, no new
        // heap entry.
        attach_or_queue(
            request(Priority::High),
            token(),
            completion(),
            &active,
            &mut pending,
            &mut heap,
            &mut seq,
        );
        assert_eq!(heap.len(), 1);
        assert_eq!(pending[&high].needs.len(), 2);

        // Same room, different priority: its own queued run.
        attach_or_queue(
            request(Priority::Viewport),
            token(),
            completion(),
            &active,
            &mut pending,
            &mut heap,
            &mut seq,
        );
        assert_eq!(heap.len(), 2);
        assert_eq!(pending[&viewport].needs.len(), 1);

        // With an active run for a key, a new request attaches to the running
        // walk instead of queueing.
        let (needs_tx, mut needs_rx) = tokio::sync::mpsc::unbounded_channel();
        active.insert(low.clone(), needs_tx);
        attach_or_queue(
            request(Priority::Low),
            token(),
            completion(),
            &active,
            &mut pending,
            &mut heap,
            &mut seq,
        );
        assert!(needs_rx.try_recv().is_ok());
        assert!(!pending.contains_key(&low));

        // If the active run has just ended (its channel is closed), the
        // request queues a fresh run instead of getting lost.
        needs_rx.close();
        attach_or_queue(
            request(Priority::Low),
            token(),
            completion(),
            &active,
            &mut pending,
            &mut heap,
            &mut seq,
        );
        assert_eq!(pending[&low].needs.len(), 1);
    }

    /// `OlderThan` stops once the oldest event in a batch is at/before the
    /// floor.
    #[test]
    fn test_stop_now() {
        let f = EventFactory::new().room(room_id!("!omelette:fromage.fr")).sender(*BOB);
        let outcome = BackPaginationOutcome {
            reached_start: false,
            events: vec![
                f.text_msg("recent").server_ts(2000).into_event(),
                f.text_msg("older").server_ts(1000).into_event(),
            ],
        };

        let floor = |ms: u32| MilliSecondsSinceUnixEpoch(ms.into());

        // Oldest event (ts 1000) newer than floor → keep going.
        assert!(!stop_now(&mut StopCondition::OlderThan(floor(500)), &outcome));
        // Oldest event at/below floor → stop.
        assert!(stop_now(&mut StopCondition::OlderThan(floor(1000)), &outcome));
        assert!(stop_now(&mut StopCondition::OlderThan(floor(1500)), &outcome));
    }

    /// A search backfill sweeps the rooms, back-paginating each until it
    /// reaches the start of the timeline.
    #[async_test]
    async fn test_search_backfill_drains_room() {
        let server = MatrixMockServer::new().await;
        let client = server
            .client_builder()
            .on_builder(|builder| builder.with_enable_automatic_back_pagination(true))
            .build()
            .await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room_id = room_id!("!omelette:fromage.fr");
        let f = EventFactory::new().room(room_id).sender(*BOB);

        let room = server.sync_joined_room(&client, room_id).await;
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        let (room_events, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
        assert!(room_events.is_empty());

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .set_timeline_limited()
                    .set_timeline_prev_batch("prev_batch"),
            )
            .await;

        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(update)) = room_cache_updates.recv()
        );
        assert_matches!(update.diffs[0], VectorDiff::Clear);

        // `/messages` returns two events and no end token → start of timeline reached.
        server
            .mock_room_messages()
            .match_from("prev_batch")
            .ok(RoomMessagesResponseTemplate::default().events(vec![
                f.text_msg("comté").event_id(event_id!("$2")),
                f.text_msg("beaufort").event_id(event_id!("$1")),
            ]))
            .mock_once()
            .mount()
            .await;

        // The single room drains on the first week and is skipped afterwards, so only
        // one `/messages` call happens (guaranteed by `mock_once`).
        event_cache
            .back_pagination_queue()
            .unwrap()
            .run_search_backfill(BackPaginationStrategy::Foreground)
            .await;

        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(update)) = room_cache_updates.recv()
        );
        assert_matches!(update.origin, EventsOrigin::Pagination);

        let mut room_events = room_events.into();
        for diff in update.diffs {
            diff.apply(&mut room_events);
        }
        assert_eq!(room_events.len(), 2);
        assert_eq!(room_events[0].event_id().unwrap(), event_id!("$1"));
        assert_eq!(room_events[1].event_id().unwrap(), event_id!("$2"));
    }

    /// Two requests for the same room at the same priority share one walk:
    /// exactly one `/messages` call serves both needs (guaranteed by
    /// `mock_once`), and both resolve.
    #[async_test]
    async fn test_two_needs_share_one_walk() {
        use std::time::Duration;

        let server = MatrixMockServer::new().await;
        // No automatic back-pagination: the only consumer of the `/messages`
        // mock below must be the test's own queue, so the `mock_once`
        // accounting is deterministic.
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room_id = room_id!("!omelette:fromage.fr");
        let f = EventFactory::new().room(room_id).sender(*BOB);

        server.sync_joined_room(&client, room_id).await;
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .set_timeline_limited()
                    .set_timeline_prev_batch("prev_batch"),
            )
            .await;

        server
            .mock_room_messages()
            .match_from("prev_batch")
            .ok(RoomMessagesResponseTemplate::default()
                .end_token("further_back")
                .events(vec![f.text_msg("gruyère").event_id(event_id!("$1"))]))
            .mock_once()
            .mount()
            .await;

        // A queue of our own (the client's automatic one is disabled).
        let queue = super::BackPaginationQueue::new(
            std::sync::Arc::downgrade(&event_cache.inner),
            4,
            client.task_monitor(),
        );

        // Both needs stop on the first non-empty batch. Enqueued back to back,
        // the second attaches to the first's run (queued or already walking).
        let need = || {
            queue.enqueue(BackPaginationRequest::read_receipt(
                room_id.to_owned(),
                |outcome: &BackPaginationOutcome| {
                    if outcome.events.is_empty() {
                        ControlFlow::Continue(())
                    } else {
                        ControlFlow::Break(())
                    }
                },
            ))
        };
        let (first, second) = (need(), need());

        let first = tokio::time::timeout(Duration::from_secs(5), first.join())
            .await
            .expect("first need must resolve");
        let second = tokio::time::timeout(Duration::from_secs(5), second.join())
            .await
            .expect("second need must resolve");

        assert_eq!(first.end, super::RoomBackPaginationEnd::StopConditionMet);
        assert_eq!(second.end, super::RoomBackPaginationEnd::StopConditionMet);
    }
}
