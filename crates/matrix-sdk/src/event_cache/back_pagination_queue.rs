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
//! The executor:
//! - runs at most [`EventCacheConfig::max_concurrent_back_paginations`]
//!   requests at once
//! - schedules by [`Priority`], higher first, FIFO within a priority
//! - never runs two requests for the same room concurrently, per-room
//!   single-flight
//! - coalesces a request for a room already queued or running at the same
//!   priority onto that run: both callers await and share its result, rather
//!   than paginating the same history twice.
//!
//! Requests are meant to be short, so a higher-priority request for a busy
//! room only waits for the current run, not a full sweep. Search requests
//! enforce this with a batch cap while latest-event and read receipt requests
//! rely on predicates that fire once a suitable candidate is loaded, so they
//! don't run indefinitely.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
    ops::ControlFlow,
    sync::{Arc, Weak},
    time::Duration,
};

use matrix_sdk_base::{sleep::sleep, task_monitor::TaskMonitor};
use matrix_sdk_common::executor::spawn;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, RoomId};
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, info, instrument, trace, warn};

use super::{EventCacheInner, caches::pagination::BackPaginationOutcome};

/// A week.
const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How far back a search backfill goes: ~3 months, one week at a time.
const MAX_BACKFILL_WEEKS: u32 = 13;

/// How many rooms are enqueued and drained together before the next batch is
/// taken, within a week of the search backfill sweep.
const ROOM_BATCH: usize = 100;

/// Number of read-receipt paginations allowed per request.
const READ_RECEIPT_MAX_BATCHES: usize = 20;

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
    /// Reactive work backing a background computation (read receipts).
    Normal,
    /// Reactive, user-facing work that wants a result promptly (latest event).
    High,
}

/// A predicate over a freshly loaded batch, deciding whether to stop.
type BatchStopPredicate = Box<dyn FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send>;

/// When a single room's back-pagination run should stop.
enum StopCondition {
    /// Stop once a batch contains an event at least this old.
    /// Used by the search backfill to fill history down to a time floor.
    OlderThan(Duration),

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
            Self::OlderThan(age) => f.debug_tuple("OlderThan").field(age).finish(),
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
    /// When to stop.
    stop: StopCondition,
    /// Number of events to request per pagination.
    batch_size: u16,
    /// Maximum number of paginations for this request (`None` = unbounded).
    max_batches: Option<usize>,
}

impl BackPaginationRequest {
    /// A search-backfill request: back-paginate down to `max_age`, at [`Low`]
    /// priority, capped at a small number of batches (a very active room is
    /// retried on the next sweep rather than left to run indefinitely).
    fn search(room_id: OwnedRoomId, max_age: Duration, batch_size: u16) -> Self {
        Self {
            room_id,
            priority: Priority::Low,
            stop: StopCondition::OlderThan(max_age),
            batch_size,
            max_batches: Some(SEARCH_MAX_BATCHES_PER_ROOM),
        }
    }

    /// A latest-event request: back-paginate until `stop` finds a candidate, at
    /// [`High`] priority.
    fn latest_event(
        room_id: OwnedRoomId,
        batch_size: u16,
        stop: impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static,
    ) -> Self {
        Self {
            room_id,
            priority: Priority::High,
            stop: StopCondition::WhenBatch(Box::new(stop)),
            batch_size,
            max_batches: None,
        }
    }

    /// A read-receipt request: back-paginate at [`Normal`] priority until
    /// `stop` fires, capped at `max_batches` (the safety net for a receipt
    /// target that never surfaces). See [`stop_on_event_ids`] for the usual
    /// predicate.
    fn read_receipt(
        room_id: OwnedRoomId,
        batch_size: u16,
        max_batches: usize,
        stop: impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static,
    ) -> Self {
        Self {
            room_id,
            priority: Priority::Normal,
            stop: StopCondition::WhenBatch(Box::new(stop)),
            batch_size,
            max_batches: Some(max_batches),
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

        Self { inner: Arc::new(BackPaginationQueueInner { sender, event_cache, _task: task }) }
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
}

/// Identifies a coalescable run i.e. a room back-paginated at a given priority.
type RequestCoalescingKey = (OwnedRoomId, Priority);

/// A request as it arrives on the queue's channel, before the scheduler has
/// assigned it a sequence number or decided whether to coalesce it.
struct SubmittedRequest {
    request: BackPaginationRequest,
    token: CancellationToken,
    completion: oneshot::Sender<BackPaginationRunResult>,
}

/// A [`BackPaginationRequest`] admitted to the scheduler's heap, with the
/// bookkeeping needed to order and run it.
struct ScheduledRequest {
    request: BackPaginationRequest,
    /// Insertion order, assigned by the scheduler, for FIFO within a priority.
    seq: u64,
    token: CancellationToken,
}

impl PartialEq for ScheduledRequest {
    fn eq(&self, other: &Self) -> bool {
        self.request.priority == other.request.priority && self.seq == other.seq
    }
}

impl Eq for ScheduledRequest {}

impl PartialOrd for ScheduledRequest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScheduledRequest {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first, then earlier `seq` first
        self.request.priority.cmp(&other.request.priority).then_with(|| other.seq.cmp(&self.seq))
    }
}

/// The executor schedules requests by priority, bounded concurrency at one run
/// per room at a time.
#[instrument(skip_all)]
async fn scheduler(
    event_cache: Weak<EventCacheInner>,
    mut receiver: mpsc::UnboundedReceiver<SubmittedRequest>,
    max_concurrent: usize,
) {
    trace!("Spawning the back-pagination queue executor");

    let mut scheduled_requests: BinaryHeap<ScheduledRequest> = BinaryHeap::new();
    let mut active_requests: Vec<(OwnedRoomId, Priority)> = Vec::new();
    let mut next_seq: u64 = 0;

    // Completion senders for every outstanding run (queued or active), keyed by
    // room and priority. A duplicate request coalesces onto the existing run by
    // adding its completion sender here rather than starting a second run. When
    // the run finishes every waiter for the key receives the same result.
    let mut waiters: HashMap<RequestCoalescingKey, Vec<oneshot::Sender<BackPaginationRunResult>>> =
        HashMap::new();

    // The executor is notified which run finished so it can free the room and
    // send the completion result to every waiter.
    let (done_tx, mut done_rx) =
        mpsc::unbounded_channel::<(RequestCoalescingKey, BackPaginationRunResult)>();

    loop {
        // Schedule as many pending requests as the concurrency budget and the
        // per-room single-flight rule allow.
        schedule(
            &event_cache,
            &mut scheduled_requests,
            &mut active_requests,
            max_concurrent,
            &done_tx,
        );

        select! {
            request = receiver.recv() => {
                match request {
                    Some(request) => {
                        let key = (request.request.room_id.clone(), request.request.priority);

                        if try_coalesce(&mut waiters, &key, request.completion) {
                            trace!(
                                room_id = %key.0,
                                priority = ?key.1,
                                "coalesced back-pagination request onto an existing run"
                            );
                            continue;
                        }

                        scheduled_requests.push(ScheduledRequest {
                            request: request.request,
                            seq: next_seq,
                            token: request.token,
                        });
                        next_seq += 1;
                    }
                    None => {
                        info!("Back-pagination queue sender closed, exiting");
                        break;
                    }
                }
            }

            Some((key, result)) = done_rx.recv() => {
                if let Some(index) = active_requests.iter().position(|(id, _)| *id == key.0) {
                    active_requests.swap_remove(index);
                }
                // Fan the single run's result out to every coalesced waiter.
                if let Some(senders) = waiters.remove(&key) {
                    for sender in senders {
                        let _ = sender.send(result);
                    }
                }
            }
        }
    }
}

/// Coalesce a new caller's `completion` onto an existing run for `key`, or
/// admit it as the first waiter of a new run.
///
/// Requests sharing a [`RequestCoalescingKey`] are functionally
/// interchangeable, so only one run is needed; extra callers wait on the same
/// result. Different priorities for the same room have different keys, so they
/// never coalesce. A busy room must still let a higher-priority request wait
/// its turn.
fn try_coalesce(
    waiters: &mut HashMap<RequestCoalescingKey, Vec<oneshot::Sender<BackPaginationRunResult>>>,
    key: &RequestCoalescingKey,
    completion: oneshot::Sender<BackPaginationRunResult>,
) -> bool {
    match waiters.get_mut(key) {
        Some(existing) => {
            existing.push(completion);
            true
        }
        None => {
            waiters.insert(key.clone(), vec![completion]);
            false
        }
    }
}

/// Pop and spawn every currently-schedulable request.
fn schedule(
    event_cache: &Weak<EventCacheInner>,
    scheduled_requests: &mut BinaryHeap<ScheduledRequest>,
    active_requests: &mut Vec<(OwnedRoomId, Priority)>,
    max_concurrent: usize,
    done_tx: &mpsc::UnboundedSender<(RequestCoalescingKey, BackPaginationRunResult)>,
) {
    for request in next_runnable(scheduled_requests, active_requests, max_concurrent) {
        let key = (request.request.room_id.clone(), request.request.priority);

        trace!(
            room_id = %key.0,
            priority = ?key.1,
            active = active_requests.len(),
            queued = scheduled_requests.len(),
            "back-pagination scheduled"
        );

        let event_cache = event_cache.clone();
        let done_tx = done_tx.clone();
        spawn(async move {
            let result = run_request(&event_cache, request.request, &request.token).await;
            // The scheduler owns the completion senders (for coalescing), so hand it the
            // result to fan out to every waiter for this key.
            let _ = done_tx.send((key, result));
        });
    }
}

/// Pick the requests that can start right now, highest priority first: bounded
/// by `max_concurrent` total in flight, and never two runs for the same room.
///
/// Picked rooms are appended to `active_requests` and requests popped but not
/// yet runnable (their room is busy) are pushed back onto the heap.
fn next_runnable(
    scheduled_requests: &mut BinaryHeap<ScheduledRequest>,
    active_requests: &mut Vec<(OwnedRoomId, Priority)>,
    max_concurrent: usize,
) -> Vec<ScheduledRequest> {
    let mut picked = Vec::new();
    let mut skipped = Vec::new();

    while active_requests.len() < max_concurrent {
        let Some(request) = scheduled_requests.pop() else {
            break;
        };

        if active_requests.iter().any(|(id, _)| *id == request.request.room_id) {
            // This room is busy, try it again next round.
            skipped.push(request);
            continue;
        }

        active_requests.push((request.request.room_id.clone(), request.request.priority));
        picked.push(request);
    }

    for request in skipped {
        scheduled_requests.push(request);
    }

    picked
}

/// Back-paginate one room until its [`StopCondition`], the start of the
/// timeline, the batch budget, or cancellation.
#[instrument(skip_all, fields(room_id = %request.room_id, priority = ?request.priority))]
async fn run_request(
    event_cache: &Weak<EventCacheInner>,
    mut request: BackPaginationRequest,
    token: &CancellationToken,
) -> BackPaginationRunResult {
    // Cancelled while still queued, nothing to do.
    if token.is_cancelled() {
        return BackPaginationRunResult { end: RoomBackPaginationEnd::Cancelled, reached: None };
    }

    // Grab an owned `RoomPagination`, dropping the caches guard immediately so
    // we don't hold the room lock across network paginations.
    let pagination = {
        let Some(inner) = event_cache.upgrade() else {
            return BackPaginationRunResult {
                end: RoomBackPaginationEnd::Cancelled,
                reached: None,
            };
        };
        match inner.all_caches_for_room(&request.room_id).await {
            Ok(caches) => caches.room.pagination(),
            Err(err) => {
                warn!("no caches for room while back-paginating: {err}");
                return BackPaginationRunResult {
                    end: RoomBackPaginationEnd::Failed,
                    reached: None,
                };
            }
        }
    };

    let mut oldest_reached: Option<MilliSecondsSinceUnixEpoch> = None;
    let mut batches = 0usize;

    let end = loop {
        if token.is_cancelled() {
            break RoomBackPaginationEnd::Cancelled;
        }

        let outcome = match pagination.run_backwards_once(request.batch_size).await {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!("back-pagination failed: {err}");
                break RoomBackPaginationEnd::Failed;
            }
        };

        if let Some(batch_oldest) = oldest_event_timestamp(&outcome) {
            oldest_reached = Some(oldest_reached.map_or(batch_oldest, |cur| cur.min(batch_oldest)));
        }

        if outcome.reached_start {
            break RoomBackPaginationEnd::ReachedTimelineStart;
        }

        if stop_now(&mut request.stop, &outcome) {
            break RoomBackPaginationEnd::StopConditionMet;
        }

        if outcome.events.is_empty() {
            break RoomBackPaginationEnd::NoDataAvailable;
        }

        batches += 1;
        if let Some(max) = request.max_batches
            && batches >= max
        {
            break RoomBackPaginationEnd::BatchLimitReached;
        }
    };

    BackPaginationRunResult { end, reached: oldest_reached }
}

/// The oldest event timestamp in a batch, if any.
fn oldest_event_timestamp(outcome: &BackPaginationOutcome) -> Option<MilliSecondsSinceUnixEpoch> {
    outcome.events.iter().filter_map(|event| event.timestamp()).min()
}

/// A stop predicate that fires as soon as a batch loads any of `targets`. With
/// no targets it never fires, so the request runs to its batch cap.
pub(crate) fn stop_on_event_ids(
    targets: HashSet<OwnedEventId>,
) -> impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static {
    move |outcome| {
        let found = outcome
            .events
            .iter()
            .any(|event| event.event_id().is_some_and(|id| targets.contains(id)));

        if found { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    }
}

/// How long ago an event's timestamp was. `None` when it's in the future or out
/// of range, i.e. clock skew between the sending server and this device; such
/// an event never satisfies an age-based stop condition.
fn age_of(ts: MilliSecondsSinceUnixEpoch) -> Option<Duration> {
    ts.to_system_time()?.elapsed().ok()
}

/// Evaluate a [`StopCondition`] against a freshly loaded batch.
fn stop_now(stop: &mut StopCondition, outcome: &BackPaginationOutcome) -> bool {
    match stop {
        StopCondition::OlderThan(max_age) => {
            oldest_event_timestamp(outcome).and_then(age_of).is_some_and(|age| age >= *max_age)
        }
        StopCondition::WhenBatch(predicate) => predicate(outcome).is_break(),
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::{ops::ControlFlow, time::Duration};

    use matrix_sdk_test::{BOB, event_factory::EventFactory};
    use ruma::{MilliSecondsSinceUnixEpoch, event_id, room_id, time::SystemTime};

    use super::{
        BackPaginationOutcome, BackPaginationRequest, Priority, ScheduledRequest, StopCondition,
        WEEK, next_runnable, stop_now, stop_on_event_ids, try_coalesce,
    };

    /// Build a queued request for a room, at a priority, with an insertion seq.
    fn queued(room_id: ruma::OwnedRoomId, priority: Priority, seq: u64) -> ScheduledRequest {
        ScheduledRequest {
            request: BackPaginationRequest {
                room_id,
                priority,
                // Scheduling tests never evaluate the stop condition.
                stop: StopCondition::WhenBatch(Box::new(|_| ControlFlow::Continue(()))),
                batch_size: 10,
                max_batches: None,
            },
            seq,
            token: tokio_util::sync::CancellationToken::new(),
        }
    }

    /// `next_runnable` serves highest priority first, then FIFO within a
    /// priority.
    #[test]
    fn test_scheduling_priority_and_fifo() {
        use std::collections::BinaryHeap;

        let (a, b, c, d) = (room_id!("!a:e"), room_id!("!b:e"), room_id!("!c:e"), room_id!("!d:e"));

        let mut scheduled_requests = BinaryHeap::new();
        // Push out of priority order, with monotonic seqs.
        scheduled_requests.push(queued(a.to_owned(), Priority::Low, 0));
        scheduled_requests.push(queued(b.to_owned(), Priority::High, 1));
        scheduled_requests.push(queued(c.to_owned(), Priority::Normal, 2));
        scheduled_requests.push(queued(d.to_owned(), Priority::High, 3));

        let mut active_requests = Vec::new();
        let picked: Vec<_> = next_runnable(&mut scheduled_requests, &mut active_requests, 10)
            .into_iter()
            .map(|r| r.request.room_id)
            .collect();

        // High first (b before d by FIFO), then Normal, then Low.
        assert_eq!(picked, vec![b.to_owned(), d.to_owned(), c.to_owned(), a.to_owned()]);
    }

    /// `next_runnable` never returns more than `max_concurrent`.
    #[test]
    fn test_scheduling_respects_concurrency_cap() {
        use std::collections::BinaryHeap;

        let mut scheduled_requests = BinaryHeap::new();
        for (i, room) in [room_id!("!a:e"), room_id!("!b:e"), room_id!("!c:e")].iter().enumerate() {
            scheduled_requests.push(queued((*room).to_owned(), Priority::Normal, i as u64));
        }

        let mut active_requests = Vec::new();
        let picked = next_runnable(&mut scheduled_requests, &mut active_requests, 2);

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
        scheduled_requests.push(queued(a.to_owned(), Priority::High, 0)); // same room as active
        scheduled_requests.push(queued(a.to_owned(), Priority::High, 1)); // and again
        scheduled_requests.push(queued(b.to_owned(), Priority::Low, 2)); // a different room

        let picked: Vec<_> = next_runnable(&mut scheduled_requests, &mut active_requests, 10)
            .into_iter()
            .map(|r| r.request.room_id)
            .collect();

        // Only `b` runs; both `a` requests stay queued (a is busy).
        assert_eq!(picked, vec![b.to_owned()]);
        assert_eq!(scheduled_requests.len(), 2);
    }

    /// The first request for a room + priority opens a new run; a later one at
    /// the same key coalesces onto it (shares its waiter list); a different
    /// priority for the same room opens its own run.
    #[test]
    fn test_coalescing() {
        use std::collections::HashMap;

        let a = room_id!("!a:e");
        let mut waiters = HashMap::new();

        let completion = || tokio::sync::oneshot::channel().0;
        let normal = (a.to_owned(), Priority::Normal);
        let high = (a.to_owned(), Priority::High);

        // First request at (a, Normal): opens a new run.
        assert!(!try_coalesce(&mut waiters, &normal, completion()));
        assert_eq!(waiters[&normal].len(), 1);

        // Second request at the same key: coalesces onto it.
        assert!(try_coalesce(&mut waiters, &normal, completion()));
        assert_eq!(waiters[&normal].len(), 2);

        // Same room, different priority: a separate run, not a coalesce.
        assert!(!try_coalesce(&mut waiters, &high, completion()));
        assert_eq!(waiters.len(), 2);
        assert_eq!(waiters[&high].len(), 1);
    }

    /// A timestamp `age` in the past.
    fn ts_ago(age: Duration) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch::from_system_time(SystemTime::now() - age).unwrap()
    }

    /// `OlderThan` stops once the oldest event in a batch is at least that old.
    #[test]
    fn test_stop_now() {
        let f = EventFactory::new().room(room_id!("!omelette:fromage.fr")).sender(*BOB);
        let outcome = BackPaginationOutcome {
            reached_start: false,
            events: vec![
                f.text_msg("recent").server_ts(ts_ago(Duration::from_secs(60))).into_event(),
                f.text_msg("older").server_ts(ts_ago(WEEK)).into_event(),
            ],
        };

        // Oldest event is a week old, under the two-week bound → keep going.
        assert!(!stop_now(&mut StopCondition::OlderThan(2 * WEEK), &outcome));
        // At/over the bound → stop.
        assert!(stop_now(&mut StopCondition::OlderThan(WEEK), &outcome));
        assert!(stop_now(&mut StopCondition::OlderThan(Duration::from_secs(3600)), &outcome));
    }

    /// `stop_on_event_ids` breaks as soon as one of its target ids is loaded;
    /// with no targets it never breaks (falls back to the batch cap).
    #[test]
    fn test_stop_on_event_ids() {
        use std::collections::HashSet;

        use ruma::owned_event_id;

        let room = room_id!("!omelette:fromage.fr");
        let f = EventFactory::new().room(room).sender(*BOB);
        let outcome = BackPaginationOutcome {
            reached_start: false,
            events: vec![
                f.text_msg("a").event_id(event_id!("$1")).into_event(),
                f.text_msg("b").event_id(event_id!("$2")).into_event(),
            ],
        };

        // A target present in the batch → stop.
        assert!(stop_on_event_ids(HashSet::from([owned_event_id!("$2")]))(&outcome).is_break());
        // No target in the batch → keep going.
        assert!(stop_on_event_ids(HashSet::from([owned_event_id!("$3")]))(&outcome).is_continue());
        // No targets at all → never stops on content.
        assert!(stop_on_event_ids(HashSet::new())(&outcome).is_continue());
    }
}
