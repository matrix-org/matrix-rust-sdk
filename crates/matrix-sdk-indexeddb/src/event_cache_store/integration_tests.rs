// Copyright 2025 The Matrix.org Foundation C.I.C.
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
// limitations under the License

use assert_matches::assert_matches;
use matrix_sdk_base::{
    event_cache::{
        Gap,
        store::{
            EventCacheStore,
            integration_tests::{check_test_event, make_test_event},
        },
    },
    linked_chunk::{ChunkContent, ChunkIdentifier, LinkedChunkId, Position, Update},
};
use matrix_sdk_test::DEFAULT_TEST_ROOM_ID;

use crate::event_cache_store::IndexeddbEventCacheStore;

pub async fn test_add_gap_chunk_and_delete_it_immediately(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let updates = vec![Update::NewGapChunk {
        previous: None,
        new: ChunkIdentifier::new(1),
        next: None,
        gap: Gap { prev_token: "cheese".to_owned() },
    }];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let updates = vec![
        Update::NewGapChunk {
            previous: Some(ChunkIdentifier::new(1)),
            new: ChunkIdentifier::new(3),
            next: None,
            gap: Gap { prev_token: "milk".to_owned() },
        },
        Update::RemoveChunk(ChunkIdentifier::new(3)),
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert_eq!(chunks.len(), 1);
}

pub async fn test_load_previous_chunk(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let event = |msg: &str| make_test_event(room_id, msg);

    // Case #1: no chunk at all, equivalent to having an nonexistent
    // `before_chunk_identifier`.
    let previous_chunk =
        store.load_previous_chunk(linked_chunk_id, ChunkIdentifier::new(153)).await.unwrap();
    assert!(previous_chunk.is_none());

    // Case #2: there is one chunk only: we request the previous on this
    // one, it doesn't exist.
    let updates =
        vec![Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None }];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let previous_chunk =
        store.load_previous_chunk(linked_chunk_id, ChunkIdentifier::new(42)).await.unwrap();
    assert!(previous_chunk.is_none());

    // Case #3: there are two chunks.
    let updates = vec![
        // new chunk before the one that exists.
        Update::NewItemsChunk {
            previous: None,
            new: ChunkIdentifier::new(7),
            next: Some(ChunkIdentifier::new(42)),
        },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(7), 0),
            items: vec![event("brigand du jorat"), event("morbier")],
        },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let previous_chunk =
        store.load_previous_chunk(linked_chunk_id, ChunkIdentifier::new(42)).await.unwrap();

    assert_matches!(previous_chunk, Some(previous_chunk) => {
        assert_eq!(previous_chunk.identifier, 7);
        assert!(previous_chunk.previous.is_none());
        assert_matches!(previous_chunk.next, Some(next) => {
            assert_eq!(next, 42);
        });
        assert_matches!(previous_chunk.content, ChunkContent::Items(items) => {
            assert_eq!(items.len(), 2);
            check_test_event(&items[0], "brigand du jorat");
            check_test_event(&items[1], "morbier");
        });
    });
}

/// Macro for generating tests for IndexedDB implementation of
/// [`EventCacheStore`]
///
/// The enclosing module must provide a function for constructing an
/// [`EventCacheStore`] which will be used in the generated tests. The function
/// must have the signature shown in the example below.
///
///
/// ## Usage Example:
/// ```no_run
/// # use matrix_sdk_base::event_cache::store::{
/// #    EventCacheStore,
/// #    EventCacheStoreError,
/// #    MemoryStore as MyStore,
/// # };
///
/// #[cfg(test)]
/// mod tests {
///     use super::{EventCacheStore, EventCacheStoreResult, MyStore};
///
///     async fn get_event_cache_store()
///     -> Result<impl EventCacheStore, EventCacheStoreError> {
///         Ok(MyStore::new())
///     }
///
///     event_cache_store_integration_tests!();
/// }
/// ```
#[macro_export]
macro_rules! indexeddb_event_cache_store_integration_tests {
    () => {
        mod indexeddb_event_cache_store_integration_tests {
            use matrix_sdk_test::async_test;

            use super::get_event_cache_store;

            #[async_test]
            async fn test_add_gap_chunk_and_delete_it_immediately() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_add_gap_chunk_and_delete_it_immediately(
                    store,
                )
                .await
            }

            #[async_test]
            async fn test_load_previous_chunk() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_load_previous_chunk(store)
                    .await
            }
        }
    };
}
