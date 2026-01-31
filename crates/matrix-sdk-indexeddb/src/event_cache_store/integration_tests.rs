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

use matrix_sdk_base::{
    event_cache::{Gap, store::EventCacheStore},
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Update},
};
use matrix_sdk_test::DEFAULT_TEST_ROOM_ID;

use crate::event_cache_store::IndexeddbEventCacheStore;

pub async fn test_linked_chunk_exists_before_referenced(store: IndexeddbEventCacheStore) {
    let room_id = *DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);

    // Fails to add the chunk because previous chunk is not in the store
    store
        .handle_linked_chunk_updates(
            linked_chunk_id,
            vec![Update::NewItemsChunk {
                previous: Some(ChunkIdentifier::new(41)),
                new: ChunkIdentifier::new(42),
                next: None,
            }],
        )
        .await
        .unwrap_err();

    // Fails to add the chunk because next chunk is not in the store
    store
        .handle_linked_chunk_updates(
            linked_chunk_id,
            vec![Update::NewItemsChunk {
                previous: None,
                new: ChunkIdentifier::new(42),
                next: Some(ChunkIdentifier::new(43)),
            }],
        )
        .await
        .unwrap_err();

    // Fails to add the chunk because previous chunk is not in the store
    store
        .handle_linked_chunk_updates(
            linked_chunk_id,
            vec![Update::NewGapChunk {
                previous: Some(ChunkIdentifier::new(41)),
                new: ChunkIdentifier::new(42),
                next: None,
                gap: Gap { prev_token: "gap".to_owned() },
            }],
        )
        .await
        .unwrap_err();

    // Fails to add the chunk because next chunk is not in the store
    store
        .handle_linked_chunk_updates(
            linked_chunk_id,
            vec![Update::NewGapChunk {
                previous: None,
                new: ChunkIdentifier::new(42),
                next: Some(ChunkIdentifier::new(43)),
                gap: Gap { prev_token: "gap".to_owned() },
            }],
        )
        .await
        .unwrap_err();

    let chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert!(chunks.is_empty());
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
            async fn test_linked_chunk_exists_before_referenced() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_exists_before_referenced(store)
                    .await
            }
        }
    };
}
