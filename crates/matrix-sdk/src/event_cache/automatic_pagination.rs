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

use std::{collections::HashMap, sync::Arc};

use ruma::OwnedRoomId;
use tokio::sync::mpsc;
use tracing::{info, instrument, trace, warn};

use crate::event_cache::EventCacheInner;

#[derive(Clone, Debug)]
pub(crate) enum AutomaticPaginationRequest {
    PaginateRoomBackwards { room_id: OwnedRoomId },
}

/// Listen to background automatic pagination requests, and execute them in
/// real-time.
#[instrument(skip_all)]
pub(super) async fn automatic_paginations_task(
    inner: Arc<EventCacheInner>,
    mut receiver: mpsc::UnboundedReceiver<AutomaticPaginationRequest>,
) {
    trace!("Spawning the automatic pagination task");

    let mut room_pagination_credits = HashMap::new();

    while let Some(request) = receiver.recv().await {
        match request {
            AutomaticPaginationRequest::PaginateRoomBackwards { room_id } => {
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
    use matrix_sdk_base::sleep::sleep;
    use matrix_sdk_test::{BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{event_id, room_id};
    use tokio::sync::mpsc;

    use crate::{
        assert_let_timeout,
        event_cache::{
            EventsOrigin, RoomEventCacheUpdate,
            automatic_pagination::AutomaticPaginationRequest::PaginateRoomBackwards,
        },
        test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    };

    impl super::super::EventCache {
        fn pagination_requests_sender(
            &self,
        ) -> Option<mpsc::UnboundedSender<super::AutomaticPaginationRequest>> {
            self.inner.automatic_pagination_requests_sender.get().cloned()
        }
    }

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

        let room = server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .set_timeline_limited()
                    .set_timeline_prev_batch("prev_batch"),
            )
            .await;

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Starting with an empty, inactive room,
        let (room_events, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
        assert!(room_events.is_empty());
        assert!(room_cache_updates.is_empty());

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
        let sender = event_cache.pagination_requests_sender().unwrap();
        sender.send(PaginateRoomBackwards { room_id: room_id.to_owned() }).unwrap();

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

        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(_)) = room_cache_updates.recv()
        );

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
        let sender = event_cache.pagination_requests_sender().unwrap();
        sender.send(PaginateRoomBackwards { room_id: room_id.to_owned() }).unwrap();

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
        sender.send(PaginateRoomBackwards { room_id: room_id.to_owned() }).unwrap();

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
