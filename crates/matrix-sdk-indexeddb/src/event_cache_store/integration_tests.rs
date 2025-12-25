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
use ruma::room_id;

use crate::{
    event_cache_store::{IndexeddbEventCacheStore, IndexeddbEventCacheStoreError},
    transaction::TransactionError,
};

pub async fn test_linked_chunk_new_items_chunk(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let updates = vec![
        Update::NewItemsChunk {
            previous: None,
            new: ChunkIdentifier::new(42),
            next: None, // Note: the store must link the next entry itself.
        },
        Update::NewItemsChunk {
            previous: Some(ChunkIdentifier::new(42)),
            new: ChunkIdentifier::new(13),
            next: Some(ChunkIdentifier::new(37)), /* But it's fine to explicitly pass
                                                   * the next link ahead of time. */
        },
        Update::NewItemsChunk {
            previous: Some(ChunkIdentifier::new(13)),
            new: ChunkIdentifier::new(37),
            next: None,
        },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert_eq!(chunks.len(), 3);

    // Chunks are ordered from smaller to bigger IDs.
    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(13));
    assert_eq!(c.previous, Some(ChunkIdentifier::new(42)));
    assert_eq!(c.next, Some(ChunkIdentifier::new(37)));
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert!(events.is_empty());
    });

    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(37));
    assert_eq!(c.previous, Some(ChunkIdentifier::new(13)));
    assert_eq!(c.next, None);
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert!(events.is_empty());
    });

    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(42));
    assert_eq!(c.previous, None);
    assert_eq!(c.next, Some(ChunkIdentifier::new(13)));
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert!(events.is_empty());
    });
}

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

pub async fn test_linked_chunk_new_gap_chunk(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let updates = vec![Update::NewGapChunk {
        previous: None,
        new: ChunkIdentifier::new(42),
        next: None,
        gap: Gap { prev_token: "raclette".to_owned() },
    }];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert_eq!(chunks.len(), 1);

    // Chunks are ordered from smaller to bigger IDs.
    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(42));
    assert_eq!(c.previous, None);
    assert_eq!(c.next, None);
    assert_matches!(c.content, ChunkContent::Gap(gap) => {
        assert_eq!(gap.prev_token, "raclette");
    });
}

pub async fn test_linked_chunk_replace_item(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let updates = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 0),
            items: vec![make_test_event(room_id, "hello"), make_test_event(room_id, "world")],
        },
        Update::ReplaceItem {
            at: Position::new(ChunkIdentifier::new(42), 1),
            item: make_test_event(room_id, "yolo"),
        },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert_eq!(chunks.len(), 1);

    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(42));
    assert_eq!(c.previous, None);
    assert_eq!(c.next, None);
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert_eq!(events.len(), 2);
        check_test_event(&events[0], "hello");
        check_test_event(&events[1], "yolo");
    });
}

pub async fn test_linked_chunk_remove_chunk(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let updates = vec![
        Update::NewGapChunk {
            previous: None,
            new: ChunkIdentifier::new(42),
            next: None,
            gap: Gap { prev_token: "raclette".to_owned() },
        },
        Update::NewGapChunk {
            previous: Some(ChunkIdentifier::new(42)),
            new: ChunkIdentifier::new(43),
            next: None,
            gap: Gap { prev_token: "fondue".to_owned() },
        },
        Update::NewGapChunk {
            previous: Some(ChunkIdentifier::new(43)),
            new: ChunkIdentifier::new(44),
            next: None,
            gap: Gap { prev_token: "tartiflette".to_owned() },
        },
        Update::RemoveChunk(ChunkIdentifier::new(43)),
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert_eq!(chunks.len(), 2);

    // Chunks are ordered from smaller to bigger IDs.
    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(42));
    assert_eq!(c.previous, None);
    assert_eq!(c.next, Some(ChunkIdentifier::new(44)));
    assert_matches!(c.content, ChunkContent::Gap(gap) => {
        assert_eq!(gap.prev_token, "raclette");
    });

    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(44));
    assert_eq!(c.previous, Some(ChunkIdentifier::new(42)));
    assert_eq!(c.next, None);
    assert_matches!(c.content, ChunkContent::Gap(gap) => {
        assert_eq!(gap.prev_token, "tartiflette");
    });
}

pub async fn test_linked_chunk_push_items(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let updates = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 0),
            items: vec![make_test_event(room_id, "hello"), make_test_event(room_id, "world")],
        },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 2),
            items: vec![make_test_event(room_id, "who?")],
        },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert_eq!(chunks.len(), 1);

    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(42));
    assert_eq!(c.previous, None);
    assert_eq!(c.next, None);
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert_eq!(events.len(), 3);
        check_test_event(&events[0], "hello");
        check_test_event(&events[1], "world");
        check_test_event(&events[2], "who?");
    });
}

pub async fn test_linked_chunk_remove_item(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let updates = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 0),
            items: vec![make_test_event(room_id, "hello"), make_test_event(room_id, "world")],
        },
        Update::RemoveItem { at: Position::new(ChunkIdentifier::new(42), 0) },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert_eq!(chunks.len(), 1);

    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(42));
    assert_eq!(c.previous, None);
    assert_eq!(c.next, None);
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert_eq!(events.len(), 1);
        check_test_event(&events[0], "world");
    });
}

pub async fn test_linked_chunk_detach_last_items(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let updates = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 0),
            items: vec![
                make_test_event(room_id, "hello"),
                make_test_event(room_id, "world"),
                make_test_event(room_id, "howdy"),
            ],
        },
        Update::DetachLastItems { at: Position::new(ChunkIdentifier::new(42), 1) },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert_eq!(chunks.len(), 1);

    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(42));
    assert_eq!(c.previous, None);
    assert_eq!(c.next, None);
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert_eq!(events.len(), 1);
        check_test_event(&events[0], "hello");
    });
}

pub async fn test_linked_chunk_start_end_reattach_items(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    // Same updates and checks as test_linked_chunk_push_items, but with extra
    // `StartReattachItems` and `EndReattachItems` updates, which must have no
    // effects.
    let updates = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 0),
            items: vec![
                make_test_event(room_id, "hello"),
                make_test_event(room_id, "world"),
                make_test_event(room_id, "howdy"),
            ],
        },
        Update::StartReattachItems,
        Update::EndReattachItems,
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let mut chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert_eq!(chunks.len(), 1);

    let c = chunks.remove(0);
    assert_eq!(c.identifier, ChunkIdentifier::new(42));
    assert_eq!(c.previous, None);
    assert_eq!(c.next, None);
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert_eq!(events.len(), 3);
        check_test_event(&events[0], "hello");
        check_test_event(&events[1], "world");
        check_test_event(&events[2], "howdy");
    });
}

pub async fn test_linked_chunk_clear(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let updates = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::NewGapChunk {
            previous: Some(ChunkIdentifier::new(42)),
            new: ChunkIdentifier::new(54),
            next: None,
            gap: Gap { prev_token: "fondue".to_owned() },
        },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 0),
            items: vec![
                make_test_event(room_id, "hello"),
                make_test_event(room_id, "world"),
                make_test_event(room_id, "howdy"),
            ],
        },
        Update::Clear,
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert!(chunks.is_empty());
}

pub async fn test_linked_chunk_multiple_rooms(store: IndexeddbEventCacheStore) {
    // Check that applying updates to one room doesn't affect the others.
    // Use the same chunk identifier in both rooms to battle-test search.
    let room_id1 = room_id!("!realcheeselovers:raclette.fr");
    let linked_chunk_id1 = LinkedChunkId::Room(room_id1);
    let updates1 = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 0),
            items: vec![
                make_test_event(room_id1, "best cheese is raclette"),
                make_test_event(room_id1, "obviously"),
            ],
        },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id1, updates1).await.unwrap();

    let room_id2 = room_id!("!realcheeselovers:fondue.ch");
    let linked_chunk_id2 = LinkedChunkId::Room(room_id2);
    let updates2 = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 0),
            items: vec![make_test_event(room_id2, "beaufort is the best")],
        },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id2, updates2).await.unwrap();

    // Check chunks from room 1.
    let mut chunks1 = store.load_all_chunks(linked_chunk_id1).await.unwrap();
    assert_eq!(chunks1.len(), 1);

    let c = chunks1.remove(0);
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert_eq!(events.len(), 2);
        check_test_event(&events[0], "best cheese is raclette");
        check_test_event(&events[1], "obviously");
    });

    // Check chunks from room 2.
    let mut chunks2 = store.load_all_chunks(linked_chunk_id2).await.unwrap();
    assert_eq!(chunks2.len(), 1);

    let c = chunks2.remove(0);
    assert_matches!(c.content, ChunkContent::Items(events) => {
        assert_eq!(events.len(), 1);
        check_test_event(&events[0], "beaufort is the best");
    });
}

pub async fn test_linked_chunk_update_is_a_transaction(store: IndexeddbEventCacheStore) {
    let linked_chunk_id = LinkedChunkId::Room(*DEFAULT_TEST_ROOM_ID);
    // Trigger a violation of the unique constraint on the (room id, chunk id)
    // couple.
    let updates = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
    ];
    let err = store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap_err();

    // The operation fails with a constraint violation error.
    assert_matches!(
        err,
        IndexeddbEventCacheStoreError::Transaction(TransactionError::DomException { .. })
    );

    // If the updates have been handled transactionally, then no new chunks should
    // have been added; failure of the second update leads to the first one being
    // rolled back.
    let chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert!(chunks.is_empty());
}

pub async fn test_load_last_chunk(store: IndexeddbEventCacheStore) {
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let event = |msg: &str| make_test_event(room_id, msg);

    // Case #1: no last chunk.
    let (last_chunk, chunk_identifier_generator) =
        store.load_last_chunk(linked_chunk_id).await.unwrap();
    assert!(last_chunk.is_none());
    assert_eq!(chunk_identifier_generator.current(), 0);

    // Case #2: only one chunk is present.
    let updates = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(42), 0),
            items: vec![event("saucisse de morteau"), event("comté")],
        },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let (last_chunk, chunk_identifier_generator) =
        store.load_last_chunk(linked_chunk_id).await.unwrap();
    assert_matches!(last_chunk, Some(last_chunk) => {
        assert_eq!(last_chunk.identifier, 42);
        assert!(last_chunk.previous.is_none());
        assert!(last_chunk.next.is_none());
        assert_matches!(last_chunk.content, ChunkContent::Items(items) => {
            assert_eq!(items.len(), 2);
            check_test_event(&items[0], "saucisse de morteau");
            check_test_event(&items[1], "comté");
        });
    });
    assert_eq!(chunk_identifier_generator.current(), 42);

    // Case #3: more chunks are present.
    let updates = vec![
        Update::NewItemsChunk {
            previous: Some(ChunkIdentifier::new(42)),
            new: ChunkIdentifier::new(7),
            next: None,
        },
        Update::PushItems {
            at: Position::new(ChunkIdentifier::new(7), 0),
            items: vec![event("fondue"), event("gruyère"), event("mont d'or")],
        },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();

    let (last_chunk, chunk_identifier_generator) =
        store.load_last_chunk(linked_chunk_id).await.unwrap();
    assert_matches!(last_chunk, Some(last_chunk) => {
        assert_eq!(last_chunk.identifier, 7);
        assert_matches!(last_chunk.previous, Some(previous) => {
            assert_eq!(previous, 42);
        });
        assert!(last_chunk.next.is_none());
        assert_matches!(last_chunk.content, ChunkContent::Items(items) => {
            assert_eq!(items.len(), 3);
            check_test_event(&items[0], "fondue");
            check_test_event(&items[1], "gruyère");
            check_test_event(&items[2], "mont d'or");
        });
    });
    assert_eq!(chunk_identifier_generator.current(), 42);
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
            async fn test_linked_chunk_new_items_chunk() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_new_items_chunk(store).await
            }

            #[async_test]
            async fn test_add_gap_chunk_and_delete_it_immediately() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_add_gap_chunk_and_delete_it_immediately(
                    store,
                )
                .await
            }

            #[async_test]
            async fn test_linked_chunk_new_gap_chunk() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_new_gap_chunk(store).await
            }

            #[async_test]
            async fn test_linked_chunk_replace_item() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_replace_item(store).await
            }

            #[async_test]
            async fn test_linked_chunk_remove_chunk() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_remove_chunk(store).await
            }

            #[async_test]
            async fn test_linked_chunk_push_items() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_push_items(store).await
            }

            #[async_test]
            async fn test_linked_chunk_remove_item() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_remove_item(store).await
            }

            #[async_test]
            async fn test_linked_chunk_detach_last_items() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_detach_last_items(store).await
            }

            #[async_test]
            async fn test_linked_chunk_start_end_reattach_items() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_start_end_reattach_items(store)
                    .await
            }

            #[async_test]
            async fn test_linked_chunk_clear() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_clear(store).await
            }

            #[async_test]
            async fn test_linked_chunk_multiple_rooms() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_multiple_rooms(store).await
            }

            #[async_test]
            async fn test_linked_chunk_update_is_a_transaction() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_update_is_a_transaction(store)
                    .await
            }

            #[async_test]
            async fn test_load_last_chunk() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_load_last_chunk(store)
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
