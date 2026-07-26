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
    use std::time::Duration;

    use matrix_sdk_test::{BOB, event_factory::EventFactory};
    use ruma::{MilliSecondsSinceUnixEpoch, event_id, room_id, time::SystemTime};

    use super::{BackPaginationOutcome, StopCondition, WEEK, stop_now, stop_on_event_ids};

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
