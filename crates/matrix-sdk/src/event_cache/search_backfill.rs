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

//! Sweeps every joined room, back-paginating message history to populate the
//! search index (and the event cache) with a few months of history.
//!
//! Requests go on the shared [`BackPaginationQueue`] at the lowest priority, so
//! reactive work (latest event, read receipts) always takes precedence.

use std::{collections::HashSet, ops::ControlFlow, time::Duration};

use matrix_sdk_base::sleep::sleep;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedRoomId, time::Instant};
use tracing::{debug, info, trace};

use super::{
    EventCache,
    back_pagination_queue::{
        BATCH_SIZE, BackPaginationQueue, BackPaginationRequest, BackPaginationRunResult, Priority,
        RoomBackPaginationEnd, oldest_event_timestamp,
    },
    caches::pagination::BackPaginationOutcome,
};

/// A week.
const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// How far back a search backfill goes: ~3 months, one week at a time.
const MAX_BACKFILL_WEEKS: u32 = 13;

/// How many rooms are enqueued and drained together before the next batch is
/// taken, within a week of the search backfill sweep.
const ROOM_BATCH: usize = 100;

/// Number of paginations allowed per room, per week, in a search backfill.
///
/// Bounds how long a single very active room can occupy a concurrency slot: a
/// room that doesn't reach the week's floor within this many batches is
/// retried on the next sweep, rather than blocking higher-priority work
/// indefinitely (there's no preemption, so requests must be self-limiting).
const SEARCH_MAX_BATCHES_PER_ROOM: usize = 10;

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

impl EventCache {
    /// Sweep every room, back-paginating message history down to a
    /// 3 months floor (`MAX_BACKFILL_WEEKS`) to populate the search index (and
    /// the event cache).
    ///
    /// Coverage is front-loaded by recency: the last week is filled for all
    /// rooms first, then the previous week, and so on, in batches of rooms.
    ///
    /// `strategy` paces how fast new rooms are introduced into the sweep:
    /// [`BackPaginationStrategy::Foreground`] spaces them out so this doesn't
    /// compete with interactive traffic.
    /// [`BackPaginationStrategy::Background`] introduces them as fast as the
    /// concurrency cap allows.
    ///
    /// No-ops if automatic back-pagination is disabled.
    pub async fn run_search_backfill(&self, strategy: BackPaginationStrategy) {
        let Some(queue) = self.back_pagination_queue() else {
            return;
        };

        let enqueue_delay = strategy.enqueue_delay();
        let started = Instant::now();

        let total_rooms = self.rooms_by_relevancy().len();
        let target_age = MAX_BACKFILL_WEEKS * WEEK;
        info!(
            ?strategy,
            total_rooms,
            weeks = MAX_BACKFILL_WEEKS,
            ?target_age,
            "search backfill started"
        );

        // Rooms that reached the start of their timeline.
        let mut drained: HashSet<OwnedRoomId> = HashSet::new();

        for week in 1..=MAX_BACKFILL_WEEKS {
            let max_age = week * WEEK;

            let rooms = self.rooms_by_relevancy();
            let rooms_to_process = rooms.iter().filter(|r| !drained.contains(*r)).count();
            debug!(
                week,
                of = MAX_BACKFILL_WEEKS,
                ?max_age,
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
                    handles.push((room_id.clone(), enqueue(&queue, room_id.clone(), max_age)));
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
            reached_age = ?target_age,
            elapsed = ?started.elapsed(),
            "search backfill finished"
        );
    }

    /// Joined room ids, most-recently-active first.
    fn rooms_by_relevancy(&self) -> Vec<OwnedRoomId> {
        let Ok(client) = self.inner.client() else {
            return Vec::new();
        };

        let mut rooms = client.joined_rooms();
        rooms.sort_by_key(|room| std::cmp::Reverse(room.recency_stamp()));
        rooms.into_iter().map(|room| room.room_id().to_owned()).collect()
    }
}

/// Enqueue one room's search back-pagination: lowest priority, stopping once a
/// batch is `max_age` old, capped so a very active room is retried on the next
/// sweep rather than left to run indefinitely.
fn enqueue(
    queue: &BackPaginationQueue,
    room_id: OwnedRoomId,
    max_age: Duration,
) -> super::back_pagination_queue::BackPaginationHandle {
    queue.enqueue(BackPaginationRequest {
        room_id,
        priority: Priority::Low,
        stop: Box::new(stop_when_older_than(max_age)),
        batch_size: BATCH_SIZE,
        max_batches: Some(SEARCH_MAX_BATCHES_PER_ROOM),
    })
}

/// A stop predicate that fires once a batch's oldest event is at least
/// `max_age` old.
fn stop_when_older_than(
    max_age: Duration,
) -> impl FnMut(&BackPaginationOutcome) -> ControlFlow<()> + Send + 'static {
    move |outcome| {
        let old_enough =
            oldest_event_timestamp(outcome).and_then(age_of).is_some_and(|age| age >= max_age);

        if old_enough { ControlFlow::Break(()) } else { ControlFlow::Continue(()) }
    }
}

/// How long ago an event's timestamp was. `None` when it's in the future or out
/// of range, i.e. clock skew between the sending server and this device; such
/// an event never satisfies an age-based stop condition.
fn age_of(ts: MilliSecondsSinceUnixEpoch) -> Option<Duration> {
    ts.to_system_time()?.elapsed().ok()
}

/// Format a timestamp as a calendar date (`YYYY-MM-DD`), for logging.
fn format_date(ts: MilliSecondsSinceUnixEpoch) -> String {
    chrono::DateTime::from_timestamp_millis(i64::from(ts.get()))
        .map(|date| date.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "?".to_owned())
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::time::Duration;

    use assert_matches::assert_matches;
    use eyeball_im::VectorDiff;
    use matrix_sdk_test::{BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{MilliSecondsSinceUnixEpoch, event_id, room_id, time::SystemTime};

    use super::{BackPaginationStrategy, WEEK, stop_when_older_than};
    use crate::{
        assert_let_timeout,
        event_cache::{BackPaginationOutcome, EventsOrigin, RoomEventCacheUpdate},
        test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    };

    /// A timestamp `age` in the past.
    fn ts_ago(age: Duration) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch::from_system_time(SystemTime::now() - age).unwrap()
    }

    /// The age predicate breaks once the oldest event in a batch is at least
    /// that old.
    #[test]
    fn test_stop_when_older_than() {
        let f = EventFactory::new().room(room_id!("!omelette:fromage.fr")).sender(*BOB);
        let outcome = BackPaginationOutcome {
            reached_start: false,
            events: vec![
                f.text_msg("recent").server_ts(ts_ago(Duration::from_secs(60))).into_event(),
                f.text_msg("older").server_ts(ts_ago(WEEK)).into_event(),
            ],
        };

        // Oldest event is a week old, under the two-week bound → keep going.
        assert!(stop_when_older_than(2 * WEEK)(&outcome).is_continue());
        // At/over the bound → stop.
        assert!(stop_when_older_than(WEEK)(&outcome).is_break());
        assert!(stop_when_older_than(Duration::from_secs(3600))(&outcome).is_break());
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
        event_cache.run_search_backfill(BackPaginationStrategy::Foreground).await;

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
}
