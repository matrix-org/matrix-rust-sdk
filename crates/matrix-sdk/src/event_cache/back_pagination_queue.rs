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
//! back-pagination requests coming from various components (search,
//! latest event, read receipt).
//!
//! Callers enqueue a [`BackPaginationRequest`] describing which room to
//! back-paginate, at which priority, and until when and they get a
//! [`BackPaginationHandle`] to await. Each consumer builds its own requests, in
//! its own module; this one only schedules and runs them.
//!
//! The executor:
//! - runs at most [`EventCacheConfig::max_concurrent_back_paginations`]
//!   requests concurrently
//! - schedules by [`Priority`], higher first, FIFO within a priority
//! - never runs two requests for the same room concurrently
//! - deduplicates by room *and* priority: a request for a room already queued
//!   or running at the same priority is coalesced onto that run, so both
//!   callers await and share its result rather than paginating the same history
//!   twice.
//!
//! A running request is never preempted, so a queued request only starts once
//! the current run for its room ends. Consumers bound their own runs with a
//! [`BackPaginationRequest::max_batches`] cap or a stop predicate that fires
//! once they have what they want.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
    ops::ControlFlow,
    sync::{Arc, Weak},
};

use matrix_sdk_base::{locks::Mutex, task_monitor::TaskMonitor};
use matrix_sdk_common::executor::{AbortOnDrop, JoinHandleExt as _, spawn};
use ruma::OwnedRoomId;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{debug, info, instrument, trace, warn};

use super::{EventCacheInner, caches::pagination::BackPaginationOutcome};

/// Number of events requested per background pagination batch.
pub(crate) const BATCH_SIZE: u16 = 30;

/// Priority of a [`BackPaginationRequest`], relative to the others in the
/// queue.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Priority {
    /// Lowest priority: will run slowly when no higher priority requests are
    /// pending.
    Low,
    /// Default priority. Higher than [`Self::Low`] and lower than
    /// [`Self::High`].
    Normal,
    /// Highest priority: will run before any other pending request.
    High,
}

/// When a single room's back-pagination run should stop: a predicate over each
/// freshly loaded batch.
///
/// Each consumer supplies its own (a suitable latest-event candidate is loaded,
/// a read receipt's target event id shows up, the batch is old enough for the
/// search backfill). A predicate that never fires leaves the run bounded only
/// by `max_batches`, the start of the timeline, or cancellation.
pub(crate) type StopCondition = Box<dyn FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send>;

/// A request to back-paginate one room, enqueued on the
/// [`BackPaginationQueue`].
pub(crate) struct BackPaginationRequest {
    /// The room to back-paginate.
    pub room_id: OwnedRoomId,
    /// Scheduling priority.
    pub priority: Priority,
    /// When to stop.
    pub stop: StopCondition,
    /// Number of events to request per pagination.
    pub batch_size: u16,
    /// Maximum number of paginations for this request (`None` = unbounded).
    pub max_batches: Option<usize>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for BackPaginationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackPaginationRequest")
            .field("room_id", &self.room_id)
            .field("priority", &self.priority)
            .field("batch_size", &self.batch_size)
            .field("max_batches", &self.max_batches)
            .finish_non_exhaustive()
    }
}

/// Why a single room's back-pagination run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackPaginationStopReason {
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
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct BackPaginationRunResult {
    /// Why the run ended.
    pub reason: BackPaginationStopReason,
}

/// Identifies a coalescable run i.e. a room back-paginated at a given priority.
type RequestCoalescingKey = (OwnedRoomId, Priority);

/// A handle to an enqueued [`BackPaginationRequest`].
/// Dropping the last handle for a request cancels it.
pub(crate) struct BackPaginationHandle {
    /// Cancels the request once every handle sharing it is dropped, unless
    /// disarmed by [`BackPaginationHandle::detach`].
    guard: Arc<DropGuard>,
    // Only read by `join`, which the consumers awaiting a result are landing with.
    #[allow(dead_code)]
    completion: Option<oneshot::Receiver<BackPaginationRunResult>>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for BackPaginationHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BackPaginationHandle").finish_non_exhaustive()
    }
}

impl BackPaginationHandle {
    /// Let the request run to completion instead of cancelling it, for callers
    /// that don't care about its result.
    pub(crate) fn detach(self) {
        // Only the last handle can disarm the cancellation; while others are alive
        // they keep the request running anyway.
        if let Some(guard) = Arc::into_inner(self.guard) {
            guard.disarm();
        }
    }

    /// Await the request's completion, returning why it ended.
    #[allow(dead_code)]
    pub(crate) async fn join(mut self) -> BackPaginationRunResult {
        let cancelled = BackPaginationRunResult { reason: BackPaginationStopReason::Cancelled };
        match self.completion.take() {
            Some(completion) => completion.await.unwrap_or(cancelled),
            None => cancelled,
        }
    }
}

/// A bounded queue of back-pagination requests, ordered by priority.
#[derive(Clone)]
pub struct BackPaginationQueue {
    inner: Arc<BackPaginationQueueInner>,
}

struct BackPaginationQueueInner {
    sender: mpsc::UnboundedSender<SchedulerEvent>,
    /// The cancellation shared by all the handles of a coalescing key, so a run
    /// is only cancelled once every caller waiting on it has dropped its
    /// handle.
    ///
    /// This only tracks handle lifetimes; the scheduler stays authoritative for
    /// whether a request actually coalesces onto an existing run.
    cancellations: Mutex<HashMap<RequestCoalescingKey, SharedCancellation>>,
    _task: matrix_sdk_base::task_monitor::BackgroundTaskHandle,
}

/// The cancellation of a single coalescable run.
struct SharedCancellation {
    /// Handed to the run, cancelled when the last `guard` below is dropped.
    token: CancellationToken,
    /// Weak, because the guard is owned by the handles: once they're all gone
    /// the token fires and this entry is stale.
    guard: Weak<DropGuard>,
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

        // The scheduler holds a sender of its own, to be handed to the runs it
        // spawns, so the channel never closes on its own: the task runs until the
        // queue is dropped, which aborts it.
        let task = task_monitor
            .spawn_infinite_task(
                "event_cache::back_pagination_queue",
                scheduler(event_cache, receiver, sender.clone(), max_concurrent),
            )
            .abort_on_drop();

        Self {
            inner: Arc::new(BackPaginationQueueInner {
                sender,
                cancellations: Mutex::new(HashMap::new()),
                _task: task,
            }),
        }
    }

    /// Enqueue a new request returning a handle to await it.
    /// A request for a room already queued or running at the same priority is
    /// coalesced onto that run rather than starting a second one.
    pub(crate) fn enqueue(
        &self,
        request: BackPaginationRequest,
    ) -> Result<BackPaginationHandle, QueueShutDown> {
        let key = (request.room_id.clone(), request.priority);
        let (token, guard) = cancellation_for(&self.inner.cancellations, key);

        let (completion_tx, completion_rx) = oneshot::channel();
        let submitted = SubmittedRequest { request, token, completion: completion_tx };

        self.inner.sender.send(SchedulerEvent::Submitted(submitted)).map_err(|_| QueueShutDown)?;

        Ok(BackPaginationHandle { guard, completion: Some(completion_rx) })
    }
}

/// The cancellation shared by every handle for `key`, created if this is the
/// first live one.
///
/// Requests that coalesce onto the same run must not be able to cancel each
/// other, so they all hold a clone of one guard and the run is only cancelled
/// once the last of them is dropped.
fn cancellation_for(
    cancellations: &Mutex<HashMap<RequestCoalescingKey, SharedCancellation>>,
    key: RequestCoalescingKey,
) -> (CancellationToken, Arc<DropGuard>) {
    let mut cancellations = cancellations.lock();

    if let Some(existing) = cancellations.get(&key)
        && let Some(guard) = existing.guard.upgrade()
    {
        return (existing.token.clone(), guard);
    }

    // Forget the keys whose handles are all gone, while we're holding the lock.
    cancellations.retain(|_, cancellation| cancellation.guard.strong_count() > 0);

    let token = CancellationToken::new();
    let guard = Arc::new(token.clone().drop_guard());
    cancellations
        .insert(key, SharedCancellation { token: token.clone(), guard: Arc::downgrade(&guard) });

    (token, guard)
}

/// The queue's executor isn't running anymore, so no new request can be
/// enqueued.
#[derive(Debug, thiserror::Error)]
#[error("the back-pagination queue executor is not running")]
pub(crate) struct QueueShutDown;

/// Everything the scheduler reacts to, on a single channel so it can wait on
/// one `recv_many` rather than selecting over two receivers.
enum SchedulerEvent {
    /// A caller enqueued a new request.
    Submitted(SubmittedRequest),
    /// A run finished: free its room and hand its result to every waiter.
    Finished(RequestCoalescingKey, BackPaginationRunResult),
}

/// A request as it arrives on the queue's channel, before the scheduler has
/// assigned it a sequence number or decided whether to coalesce it.
struct SubmittedRequest {
    request: BackPaginationRequest,
    token: CancellationToken,
    completion: oneshot::Sender<BackPaginationRunResult>,
}

/// A [`BackPaginationRequest`] admitted to the scheduler, holding the sequence
/// number and request details.
struct PendingRequest {
    request: BackPaginationRequest,
    /// Insertion order, assigned by the scheduler, for FIFO within a priority.
    seq: u64,
    token: CancellationToken,
}

impl PartialEq for PendingRequest {
    fn eq(&self, other: &Self) -> bool {
        self.request.priority == other.request.priority && self.seq == other.seq
    }
}

impl Eq for PendingRequest {}

impl PartialOrd for PendingRequest {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PendingRequest {
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
    mut receiver: mpsc::UnboundedReceiver<SchedulerEvent>,
    sender: mpsc::UnboundedSender<SchedulerEvent>,
    max_concurrent: usize,
) {
    trace!("Spawning the back-pagination queue executor");

    let mut pending_requests: BinaryHeap<PendingRequest> = BinaryHeap::new();
    // The tasks of the currently running requests, keyed by room: also the set of
    // rooms that can't take another run right now. Dropping the scheduler aborts
    // all of them.
    let mut active_requests: HashMap<OwnedRoomId, AbortOnDrop<()>> = HashMap::new();
    let mut next_seq: u64 = 0;

    // Completion senders for every request the scheduler knows about (queued or
    // running), keyed by room and priority. A duplicate request coalesces onto the
    // existing run by adding its completion sender here rather than starting a
    // second run. When the run finishes every waiter for the key receives the same
    // result.
    let mut waiters: HashMap<RequestCoalescingKey, Vec<oneshot::Sender<BackPaginationRunResult>>> =
        HashMap::new();

    // At most `max_concurrent` runs are in flight, so there's never a reason to
    // drain more than that in one go.
    let batch_limit = max_concurrent.max(1);
    let mut events = Vec::with_capacity(batch_limit);

    loop {
        // Schedule as many pending requests as the concurrency budget allows, never
        // starting a second run for a room that's already running one.
        schedule(
            &event_cache,
            &mut pending_requests,
            &mut active_requests,
            max_concurrent,
            &sender,
        );

        // Unreachable while this task holds `sender`, but guards against a hot loop
        // if that ever stops being true.
        if receiver.recv_many(&mut events, batch_limit).await == 0 {
            info!("Back-pagination queue channel closed, exiting");
            break;
        }

        for event in events.drain(..) {
            match event {
                SchedulerEvent::Submitted(submitted) => {
                    let key = (submitted.request.room_id.clone(), submitted.request.priority);

                    if try_coalesce(&mut waiters, &key, submitted.completion) {
                        trace!(
                            room_id = %key.0,
                            priority = ?key.1,
                            "coalesced back-pagination request onto an existing run"
                        );
                        continue;
                    }

                    pending_requests.push(PendingRequest {
                        request: submitted.request,
                        seq: next_seq,
                        token: submitted.token,
                    });
                    next_seq += 1;
                }

                SchedulerEvent::Finished(key, result) => {
                    active_requests.remove(&key.0);
                    // Fan the single run's result out to every coalesced waiter.
                    if let Some(senders) = waiters.remove(&key) {
                        for waiter in senders {
                            let _ = waiter.send(result);
                        }
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
    pending_requests: &mut BinaryHeap<PendingRequest>,
    active_requests: &mut HashMap<OwnedRoomId, AbortOnDrop<()>>,
    max_concurrent: usize,
    sender: &mpsc::UnboundedSender<SchedulerEvent>,
) {
    for request in next_runnable(pending_requests, active_requests, max_concurrent) {
        let key = (request.request.room_id.clone(), request.request.priority);

        trace!(
            room_id = %key.0,
            priority = ?key.1,
            active = active_requests.len(),
            queued = pending_requests.len(),
            "back-pagination scheduled"
        );

        let room_id = key.0.clone();
        let event_cache = event_cache.clone();
        let sender = sender.clone();
        let task = spawn(async move {
            let result = run_request(&event_cache, request.request, &request.token).await;
            // The scheduler owns the completion senders (for coalescing), so hand it the
            // result to fan out to every waiter for this key.
            let _ = sender.send(SchedulerEvent::Finished(key, result));
        });

        active_requests.insert(room_id, task.abort_on_drop());
    }
}

/// Pick the requests that can start right now, highest priority first: bounded
/// by `max_concurrent` total in flight, and never two runs for the same room.
///
/// Requests popped but not yet runnable (their room is busy) are pushed back
/// onto the heap.
// Generic over the map's value type so the scheduling tests don't need a
// runtime to build a task handle; only the keys matter here.
fn next_runnable<T>(
    pending_requests: &mut BinaryHeap<PendingRequest>,
    active_requests: &HashMap<OwnedRoomId, T>,
    max_concurrent: usize,
) -> Vec<PendingRequest> {
    let mut picked: Vec<PendingRequest> = Vec::new();
    let mut skipped = Vec::new();

    while active_requests.len() + picked.len() < max_concurrent {
        let Some(request) = pending_requests.pop() else {
            break;
        };

        let room_id = &request.request.room_id;
        if active_requests.contains_key(room_id)
            || picked.iter().any(|other| other.request.room_id == *room_id)
        {
            // This room is busy, try it again next round.
            skipped.push(request);
            continue;
        }

        picked.push(request);
    }

    for request in skipped {
        pending_requests.push(request);
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
        return BackPaginationRunResult { reason: BackPaginationStopReason::Cancelled };
    }

    // Grab an owned `RoomPagination`, dropping the caches guard immediately so
    // we don't hold the room lock across network paginations.
    let pagination = {
        let Some(inner) = event_cache.upgrade() else {
            return BackPaginationRunResult { reason: BackPaginationStopReason::Cancelled };
        };
        match inner.all_caches_for_room(&request.room_id).await {
            Ok(caches) => caches.room.pagination(),
            Err(err) => {
                warn!("no caches for room while back-paginating: {err}");
                return BackPaginationRunResult { reason: BackPaginationStopReason::Failed };
            }
        }
    };

    let mut batches = 0usize;

    let reason = loop {
        if token.is_cancelled() {
            break BackPaginationStopReason::Cancelled;
        }

        let outcome = match pagination.run_backwards_once(request.batch_size).await {
            Ok(outcome) => outcome,
            Err(err) => {
                warn!("back-pagination failed: {err}");
                break BackPaginationStopReason::Failed;
            }
        };

        // Reaching the start of the timeline can still come with a last batch of
        // events, so let the stop condition see it before ending the run.
        if (request.stop)(&outcome).is_break() {
            break BackPaginationStopReason::StopConditionMet;
        }

        if outcome.reached_start {
            break BackPaginationStopReason::ReachedTimelineStart;
        }

        if outcome.events.is_empty() {
            break BackPaginationStopReason::NoDataAvailable;
        }

        batches += 1;
        if let Some(max) = request.max_batches
            && batches >= max
        {
            break BackPaginationStopReason::BatchLimitReached;
        }
    };

    debug!(?reason, "back-pagination run finished");

    BackPaginationRunResult { reason }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::{collections::HashMap, ops::ControlFlow};

    use matrix_sdk_base::locks::Mutex;
    use ruma::room_id;

    use super::{
        BackPaginationRequest, PendingRequest, Priority, cancellation_for, next_runnable,
        try_coalesce,
    };

    /// Build a queued request for a room, at a priority, with an insertion seq.
    fn queued(room_id: ruma::OwnedRoomId, priority: Priority, seq: u64) -> PendingRequest {
        PendingRequest {
            request: BackPaginationRequest {
                room_id,
                priority,
                // Scheduling tests never evaluate the stop condition.
                stop: Box::new(|_| ControlFlow::Continue(())),
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

        let mut pending_requests = BinaryHeap::new();
        // Push out of priority order, with monotonic seqs.
        pending_requests.push(queued(a.to_owned(), Priority::Low, 0));
        pending_requests.push(queued(b.to_owned(), Priority::High, 1));
        pending_requests.push(queued(c.to_owned(), Priority::Normal, 2));
        pending_requests.push(queued(d.to_owned(), Priority::High, 3));

        let active_requests: HashMap<_, ()> = HashMap::new();
        let picked: Vec<_> = next_runnable(&mut pending_requests, &active_requests, 10)
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

        let mut pending_requests = BinaryHeap::new();
        for (i, room) in [room_id!("!a:e"), room_id!("!b:e"), room_id!("!c:e")].iter().enumerate() {
            pending_requests.push(queued((*room).to_owned(), Priority::Normal, i as u64));
        }

        let active_requests: HashMap<_, ()> = HashMap::new();
        let picked = next_runnable(&mut pending_requests, &active_requests, 2);

        assert_eq!(picked.len(), 2);
        // The third request stays queued.
        assert_eq!(pending_requests.len(), 1);
    }

    /// `next_runnable` won't start a room that's already active, nor two runs
    /// for the same room in one pass.
    #[test]
    fn test_scheduling_per_room_single_flight() {
        use std::collections::BinaryHeap;

        let (a, b) = (room_id!("!a:e"), room_id!("!b:e"));

        // `a` is already running.
        let active_requests = HashMap::from([(a.to_owned(), ())]);

        let mut pending_requests = BinaryHeap::new();
        pending_requests.push(queued(a.to_owned(), Priority::High, 0)); // same room as active
        pending_requests.push(queued(a.to_owned(), Priority::High, 1)); // and again
        pending_requests.push(queued(b.to_owned(), Priority::Low, 2)); // a different room

        let picked: Vec<_> = next_runnable(&mut pending_requests, &active_requests, 10)
            .into_iter()
            .map(|r| r.request.room_id)
            .collect();

        // Only `b` runs; both `a` requests stay queued (a is busy).
        assert_eq!(picked, vec![b.to_owned()]);
        assert_eq!(pending_requests.len(), 2);
    }

    /// Every handle for a room + priority shares one cancellation, so a caller
    /// dropping its handle can't cancel a run others are still waiting on.
    #[test]
    fn test_cancellation_is_shared_per_key() {
        let cancellations = Mutex::new(HashMap::new());
        let key = (room_id!("!a:e").to_owned(), Priority::Normal);

        let (first_token, first_guard) = cancellation_for(&cancellations, key.clone());
        let (second_token, second_guard) = cancellation_for(&cancellations, key.clone());

        // One run, so one token for both callers.
        assert!(!first_token.is_cancelled());
        drop(first_guard);
        assert!(!first_token.is_cancelled());
        assert!(!second_token.is_cancelled());

        // The last handle to go cancels the run.
        drop(second_guard);
        assert!(first_token.is_cancelled());
        assert!(second_token.is_cancelled());

        // Once they're all gone the key is stale, so the next caller gets a fresh
        // token rather than an already-cancelled one.
        let (third_token, _third_guard) = cancellation_for(&cancellations, key);
        assert!(!third_token.is_cancelled());

        // A different priority for the same room is a different run.
        let other = (room_id!("!a:e").to_owned(), Priority::High);
        let (other_token, other_guard) = cancellation_for(&cancellations, other);
        drop(other_guard);
        assert!(other_token.is_cancelled());
        assert!(!third_token.is_cancelled());
    }

    /// The first request for a room + priority opens a new run; a later one at
    /// the same key coalesces onto it (shares its waiter list); a different
    /// priority for the same room opens its own run.
    #[test]
    fn test_coalescing() {
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
}
