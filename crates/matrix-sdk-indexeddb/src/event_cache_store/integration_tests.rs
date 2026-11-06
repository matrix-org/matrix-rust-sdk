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

use std::collections::HashMap;

use indexed_db_futures::{Build, BuildSerde, query_source::QuerySource};
use matrix_sdk_base::{
    event_cache::store::EventCacheStore,
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Update},
};
use matrix_sdk_test::{DEFAULT_TEST_ROOM_ID, event_factory::EventFactory};
use ruma::{OwnedEventId, event_id, user_id};
use web_sys::IdbTransactionMode;

use crate::{
    event_cache_store::{
        IndexeddbEventCacheStore, migrations::current::keys,
        serializer::indexed_types::IndexedEvent, types::Event,
    },
    serializer::indexed_type::traits::Indexed,
};

pub async fn test_linked_chunk_update_is_a_transaction(store: IndexeddbEventCacheStore) {
    let linked_chunk_id = LinkedChunkId::Room(*DEFAULT_TEST_ROOM_ID);
    // Trigger a violation of the unique constraint on the (room id, chunk id)
    // couple.
    let updates = vec![
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
    ];
    store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap_err();

    // If the updates have been handled transactionally, then no new chunks
    // should have been added; failure of the second update leads to the first
    // one being rolled back.
    let chunks = store.load_all_chunks(linked_chunk_id).await.unwrap();
    assert!(chunks.is_empty());
}

pub async fn test_is_state_event_field(store: IndexeddbEventCacheStore) {
    let room_id = *DEFAULT_TEST_ROOM_ID;
    let f = EventFactory::new().room(room_id).sender(user_id!("@alice:localhost"));

    let message_id = event_id!("$message");
    let empty_state_key_id = event_id!("$empty_state_key");
    let state_key_id = event_id!("$state_key");

    // A message-like event, without a `state_key`.
    store.save_event(room_id, f.text_msg("hello").event_id(message_id).into_event()).await.unwrap();
    // A state event with an empty `state_key`.
    store
        .save_event(
            room_id,
            f.room_topic("topic").state_key("").event_id(empty_state_key_id).into_event(),
        )
        .await
        .unwrap();
    // A state event with a non-empty `state_key`.
    store
        .save_event(
            room_id,
            f.member(user_id!("@bob:localhost")).event_id(state_key_id).into_event(),
        )
        .await
        .unwrap();

    // Read the raw indexed events, to check the stored `is_state_event` field.
    let transaction = store
        .inner
        .transaction(keys::EVENTS)
        .with_mode(IdbTransactionMode::Readonly)
        .build()
        .unwrap();
    let indexed_events: Vec<IndexedEvent> = transaction
        .object_store(keys::EVENTS)
        .unwrap()
        .get_all()
        .serde()
        .unwrap()
        .await
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    let is_state_event: HashMap<OwnedEventId, bool> = indexed_events
        .into_iter()
        .map(|indexed| {
            let is_state_event = indexed.is_state_event;
            let event = Event::from_indexed(indexed, store.serializer.inner()).unwrap();
            (event.event_id().unwrap().to_owned(), is_state_event)
        })
        .collect();

    assert_eq!(is_state_event.len(), 3);
    assert!(!is_state_event[message_id]);
    assert!(is_state_event[empty_state_key_id]);
    assert!(is_state_event[state_key_id]);
}

/// Macro for generating tests for IndexedDB implementation of
/// [`EventCacheStore`]
///
/// The enclosing module must provide a function for constructing an
/// [`EventCacheStore`] which will be used in the generated tests. The function
/// must have the signature shown in the example below.
///
/// ## Usage example
///
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
            async fn test_linked_chunk_update_is_a_transaction() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_linked_chunk_update_is_a_transaction(store)
                    .await
            }

            #[async_test]
            async fn test_is_state_event_field() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_is_state_event_field(store).await
            }
        }
    };
}
