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

use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

use matrix_sdk_base::task_monitor::{BackgroundTaskHandle, TaskMonitor};
use ruma::{OwnedRoomId, RoomId};
use tokio::sync::mpsc;
use tracing::{info, instrument, trace, warn};

use crate::event_cache::EventCacheInner;

/// State for running paginations in background tasks.
///
/// Shallow type, can be cloned cheaply.
#[derive(Clone)]
pub struct AutomaticPagination {
    inner: Arc<AutomaticPaginationInner>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for AutomaticPagination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutomaticPagination").finish_non_exhaustive()
    }
}

impl AutomaticPagination {
    /// Create a new [`AutomaticPagination`], spawning the background task to
    /// handle incoming requests to run background paginations.
    pub(super) fn new(event_cache: Weak<EventCacheInner>, task_monitor: &TaskMonitor) -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();

        let task = task_monitor.spawn_background_task(
            "event_cache::automatic_paginations_task",
            automatic_paginations_task(event_cache, receiver),
        );

        Self { inner: Arc::new(AutomaticPaginationInner { _task: task, sender }) }
    }

    /// Request a single back-pagination to happen in the background for the
    /// given room.
    ///
    /// Returns false, if the request couldn't be sent.
    pub fn run_once(&self, room_id: &RoomId) -> bool {
        // We don't want to do anything with the error type, as it only includes the
        // request we just created, and not much more; there's no guarantee that
        // retrying sending it would succeed, so let it drop, and report the
        // result as a boolean, for informative purposes.
        self.inner
            .sender
            .send(AutomaticPaginationRequest::PaginateRoomBackwards { room_id: room_id.to_owned() })
            .is_ok()
    }
}

struct AutomaticPaginationInner {
    /// The task used to handle automatic pagination requests.
    _task: BackgroundTaskHandle,

    /// A sender for automatic pagination requests, that is shared with every
    /// room.
    ///
    /// It's a `OnceLock` because its initialization is deferred to
    /// [`EventCache::subscribe`].
    sender: mpsc::UnboundedSender<AutomaticPaginationRequest>,
}

#[derive(Clone, Debug)]
enum AutomaticPaginationRequest {
    PaginateRoomBackwards { room_id: OwnedRoomId },
}

/// Listen to background automatic pagination requests, and execute them in
/// real-time.
#[instrument(skip_all)]
async fn automatic_paginations_task(
    inner: Weak<EventCacheInner>,
    mut receiver: mpsc::UnboundedReceiver<AutomaticPaginationRequest>,
) {
    trace!("Spawning the automatic pagination task");

    let mut room_pagination_credits = HashMap::new();

    while let Some(request) = receiver.recv().await {
        match request {
            AutomaticPaginationRequest::PaginateRoomBackwards { room_id } => {
                let Some(inner) = inner.upgrade() else {
                    // The event cache has been dropped, exit the task.
                    break;
                };

                let config = *inner.config.read().unwrap();

                let credits = room_pagination_credits
                    .entry(room_id.clone())
                    .or_insert(config.room_pagination_per_room_credit);

                if *credits == 0 {
                    trace!(for_room = %room_id, "No more credits to paginate this room in the background, skipping");
                    continue;
                }

                let pagination = match inner.all_caches_for_room(&room_id).await {
                    Ok(caches) => caches.room.pagination(),
                    Err(err) => {
                        warn!(for_room = %room_id, "Failed to get the `Caches`: {err}");
                        continue;
                    }
                };

                trace!(for_room = %room_id, "automatic backpagination triggered");

                match pagination.run_backwards_once(config.room_pagination_batch_size).await {
                    Ok(outcome) => {
                        // Pagination requests must be idempotent, so we only decrement credits if
                        // we actually paginated something new.
                        if !outcome.reached_start || !outcome.events.is_empty() {
                            *credits -= 1;
                        }
                    }

                    Err(err) => {
                        warn!(for_room = %room_id, "Failed to run background pagination: {err}");
                        // Don't decrement credits in this case, to allow a
                        // retry later.
                    }
                }
            }
        }
    }

    // The sender has shut down, exit.
    info!("Closing the automatic pagination task because receiver closed");
}

// MatrixMockServer et al. aren't available on wasm.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::time::Duration;

    use assert_matches::assert_matches;
    use eyeball_im::VectorDiff;
    use matrix_sdk_base::sleep::sleep;
    use matrix_sdk_test::{BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{event_id, room_id};

    use crate::{
        assert_let_timeout,
        event_cache::{EventsOrigin, RoomEventCacheUpdate},
        test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    };

    /// Test that we can send automatic pagination requests.
    #[async_test]
    async fn test_background_room_paginations() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.config_mut().experimental_auto_backpagination = true;
        event_cache.subscribe().unwrap();

        let room_id = room_id!("!omelette:fromage.fr");
        let f = EventFactory::new().room(room_id).sender(*BOB);

        let room = server.sync_joined_room(&client, room_id).await;

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Starting with an empty, inactive room,
        let (room_events, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
        assert!(room_events.is_empty());
        assert!(room_cache_updates.is_empty());

        // We get a gappy sync (so as to have a previous-batch token),
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .set_timeline_limited()
                    .set_timeline_prev_batch("prev_batch"),
            )
            .await;

        {
            assert_let_timeout!(
                Ok(RoomEventCacheUpdate::UpdateTimelineEvents(update)) = room_cache_updates.recv()
            );
            assert_eq!(update.diffs.len(), 1);
            assert_matches!(update.diffs[0], VectorDiff::Clear);
            assert_matches!(update.origin, EventsOrigin::Sync);
        }

        // Set up the mock for /messages,
        server
            .mock_room_messages()
            .ok(RoomMessagesResponseTemplate::default().events(vec![
                f.text_msg("comté").event_id(event_id!("$2")),
                f.text_msg("beaufort").event_id(event_id!("$1")),
            ]))
            .mock_once()
            .mount()
            .await;

        // Send a request for a background pagination,
        let automatic_pagination_api = event_cache.automatic_pagination().unwrap();
        assert!(automatic_pagination_api.run_once(room_id));

        // The room pagination happens in the background.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(update)) = room_cache_updates.recv()
        );
        assert_eq!(update.diffs.len(), 1);

        assert_matches!(update.origin, EventsOrigin::Pagination);

        let mut room_events = room_events.into();
        for diff in update.diffs {
            diff.apply(&mut room_events);
        }

        assert_eq!(room_events.len(), 2);
        assert_eq!(room_events[0].event_id().unwrap(), event_id!("$1"));
        assert_eq!(room_events[1].event_id().unwrap(), event_id!("$2"));

        // And there's no more updates.
        assert!(room_cache_updates.is_empty());
    }

    /// Test that the credit system works.
    #[async_test]
    async fn test_room_pagination_respects_credits_system() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.config_mut().experimental_auto_backpagination = true;

        // Only allow 1 background pagination per room, to test that the credit system
        // is properly taken into account.
        event_cache.config_mut().room_pagination_per_room_credit = 1;
        event_cache.subscribe().unwrap();

        let room_id = room_id!("!omelette:fromage.fr");
        let f = EventFactory::new().room(room_id).sender(*BOB);

        let room = server.sync_joined_room(&client, room_id).await;
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Starting with an empty, inactive room,
        let (room_events, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
        assert!(room_events.is_empty());
        assert!(room_cache_updates.is_empty());

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .set_timeline_limited()
                    .set_timeline_prev_batch("prev_batch"),
            )
            .await;

        {
            assert_let_timeout!(
                Ok(RoomEventCacheUpdate::UpdateTimelineEvents(update)) = room_cache_updates.recv()
            );
            assert_eq!(update.diffs.len(), 1);
            assert_matches!(update.diffs[0], VectorDiff::Clear);
            assert_matches!(update.origin, EventsOrigin::Sync);
        }

        // Set up the mock for /messages, so that it returns another prev-batch token,
        server
            .mock_room_messages()
            .match_from("prev_batch")
            .ok(RoomMessagesResponseTemplate::default()
                .events(vec![
                    f.text_msg("comté").event_id(event_id!("$2")),
                    f.text_msg("beaufort").event_id(event_id!("$1")),
                ])
                .end_token("prev_batch_2"))
            .mock_once()
            .mount()
            .await;

        // Send a request for a background pagination,
        let automatic_pagination_api = event_cache.automatic_pagination().unwrap();
        assert!(automatic_pagination_api.run_once(room_id));

        // The room pagination happens in the background.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(update)) = room_cache_updates.recv()
        );
        assert_eq!(update.diffs.len(), 1);

        assert_matches!(update.origin, EventsOrigin::Pagination);

        let mut room_events = room_events.into();
        for diff in update.diffs {
            diff.apply(&mut room_events);
        }

        assert_eq!(room_events.len(), 2);
        assert_eq!(room_events[0].event_id().unwrap(), event_id!("$1"));
        assert_eq!(room_events[1].event_id().unwrap(), event_id!("$2"));

        // And there's no more updates yet.
        assert!(room_cache_updates.is_empty());

        // One can send another request to back-paginate…
        assert!(automatic_pagination_api.run_once(room_id));

        sleep(Duration::from_millis(300)).await;
        // But it doesn't happen, because we don't have enough credits for automatic
        // backpagination.
        assert!(room_cache_updates.is_empty());

        // We can still manually backpaginate with success, though.
        server
            .mock_room_messages()
            .match_from("prev_batch_2")
            .ok(RoomMessagesResponseTemplate::default())
            .mock_once()
            .mount()
            .await;

        let outcome = room_event_cache.pagination().run_backwards_once(30).await.unwrap();
        assert!(outcome.reached_start);
    }
}
