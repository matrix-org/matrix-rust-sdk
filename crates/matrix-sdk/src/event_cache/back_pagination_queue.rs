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

use matrix_sdk_base::task_monitor::TaskMonitor;
use matrix_sdk_common::executor::spawn;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId};
use tokio::{
    select,
    sync::{mpsc, oneshot},
};
use tokio_util::sync::{CancellationToken, DropGuard};
use tracing::{info, instrument, trace, warn};

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
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct BackPaginationRunResult {
    /// Why the run ended.
    pub end: RoomBackPaginationEnd,
    /// The oldest event timestamp reached, if any events were loaded.
    pub reached: Option<MilliSecondsSinceUnixEpoch>,
}

/// A handle to an enqueued [`BackPaginationRequest`].
/// Dropping the handle cancels the request.
pub(crate) struct BackPaginationHandle {
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
    pub(crate) async fn join(mut self) -> BackPaginationRunResult {
        let cancelled =
            BackPaginationRunResult { end: RoomBackPaginationEnd::Cancelled, reached: None };
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
    sender: mpsc::UnboundedSender<SubmittedRequest>,
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
            scheduler(event_cache, receiver, max_concurrent),
        );

        Self { inner: Arc::new(BackPaginationQueueInner { sender, _task: task }) }
    }

    /// Enqueue a new request returning a handle to await it.
    /// A request for a room already queued or running at the same priority is
    /// coalesced onto that run rather than starting a second one.
    pub(crate) fn enqueue(&self, request: BackPaginationRequest) -> BackPaginationHandle {
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

/// A [`BackPaginationRequest`] admitted to the scheduler, holding the sequence
/// number and request details.
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

    // Completion senders for every request the scheduler knows about (queued or
    // running), keyed by room and priority. A duplicate request coalesces onto the
    // existing run by adding its completion sender here rather than starting a
    // second run. When the run finishes every waiter for the key receives the same
    // result.
    let mut waiters: HashMap<RequestCoalescingKey, Vec<oneshot::Sender<BackPaginationRunResult>>> =
        HashMap::new();

    // The executor is notified which run finished so it can free the room and
    // send the completion result to every waiter.
    let (done_tx, mut done_rx) =
        mpsc::unbounded_channel::<(RequestCoalescingKey, BackPaginationRunResult)>();

    loop {
        // Schedule as many pending requests as the concurrency budget allows, never
        // starting a second run for a room that's already running one.
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
            waiters.insert(key.to_owned(), vec![completion]);
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

    scheduled_requests.extend(skipped);

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
            Ok(caches) => caches.room().pagination(),
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

        if (request.stop)(&outcome).is_break() {
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
pub(crate) fn oldest_event_timestamp(
    outcome: &BackPaginationOutcome,
) -> Option<MilliSecondsSinceUnixEpoch> {
    outcome.events.iter().filter_map(|event| event.timestamp()).min()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::ops::ControlFlow;

    use ruma::room_id;

    use super::{BackPaginationRequest, Priority, ScheduledRequest, next_runnable, try_coalesce};

    /// Build a queued request for a room, at a priority, with an insertion seq.
    fn queued(room_id: ruma::OwnedRoomId, priority: Priority, seq: u64) -> ScheduledRequest {
        ScheduledRequest {
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
}
