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
//! [`BackPaginationHandle`] to await. Each consumer builds its own requests, in
//! its own module; this one only schedules and runs them.
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
//! room only waits for the current run, not a full sweep. It's up to each
//! consumer to keep its own requests short, with a batch cap or a stop
//! predicate that fires once it has what it wants.

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
// Not every priority has a consumer yet: read receipts is `Normal`, while the
// search backfill (`Low`) and the latest-event resolver (`High`) are landing
// separately.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Priority {
    /// Bulk work with no deadline (search backfill).
    Low,
    /// Reactive work backing a background computation (read receipts).
    Normal,
    /// Reactive, user-facing work that wants a result promptly (latest event).
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
    pub(crate) room_id: OwnedRoomId,
    /// Scheduling priority.
    pub(crate) priority: Priority,
    /// When to stop.
    pub(crate) stop: StopCondition,
    /// Number of events to request per pagination.
    pub(crate) batch_size: u16,
    /// Maximum number of paginations for this request (`None` = unbounded).
    pub(crate) max_batches: Option<usize>,
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
// Read receipts is fire-and-forget and ignores this; the consumers that read it
// (the latest-event resolver, the search backfill) are landing separately.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct BackPaginationRunResult {
    /// Why the run ended.
    pub(crate) end: RoomBackPaginationEnd,
    /// The oldest event timestamp reached, if any events were loaded.
    pub(crate) reached: Option<MilliSecondsSinceUnixEpoch>,
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

