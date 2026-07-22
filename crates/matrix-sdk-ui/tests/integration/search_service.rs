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

use std::time::Duration;

use assert_matches2::assert_let;
use matrix_sdk::{
    event_cache::BackPaginationStrategy,
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
};
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use matrix_sdk_ui::search_service::{ResultType, SearchService};
use ruma::{event_id, room_id, user_id};

/// A search backfill pulls in a room's history, the search indexing task
/// indexes it, and the public `SearchService` then returns a term that existed
/// only in that history. Exercises the whole client-facing chain:
/// back-pagination queue -> event cache -> search indexing task ->
/// `SearchService`.
#[async_test]
async fn test_search_backfill_makes_history_searchable() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    // Enable before subscribing, so the back-pagination queue is spawned.
    event_cache.config_mut().experimental_auto_back_pagination = true;
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let sender = user_id!("@bob:example.org");
    let f = EventFactory::new().room(room_id).sender(sender);

    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_room_events, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();

    // Add a gappy sync i.e. an empty in-memory timeline, but a previous-batch
    // token to paginate from.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .set_timeline_limited()
                .set_timeline_prev_batch("prev_batch"),
        )
        .await;
    let _ = room_cache_updates.recv().await; // Drain the `Clear`.

    // A searchable message that exists only in history, behind the gap.
    server
        .mock_room_messages()
        .match_from("prev_batch")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("beaufort cheese").event_id(event_id!("$1"))]))
        .mock_once()
        .mount()
        .await;

    client
        .event_cache()
        .back_pagination_queue()
        .unwrap()
        .run_search_backfill(BackPaginationStrategy::Foreground)
        .await;

    // Query through the public search API. Indexing runs asynchronously off the
    // linked-chunk updates, so poll until the backfilled event shows up.
    let search = SearchService::new(client.clone());
    let mut results = Vec::new();
    for _ in 0..40 {
        search.set_query("beaufort".to_owned()).await.unwrap();
        results = search.results().await;
        if !results.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert_eq!(results.len(), 1);
    assert_let!(ResultType::Message(message) = &results[0]);
    assert_eq!(message.event_id, event_id!("$1"));
    assert_eq!(message.room_id, room_id);
}
