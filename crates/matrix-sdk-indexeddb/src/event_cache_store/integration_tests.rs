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
    event_cache::store::EventCacheStore,
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Update},
};
use matrix_sdk_test::{DEFAULT_TEST_ROOM_ID, event_factory::EventFactory};
use ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, event_id, user_id};
use web_sys::IdbTransactionMode;

use crate::{
    event_cache_store::{
        IndexeddbEventCacheStore, migrations::current::keys,
        serializer::indexed_types::IndexedEventStateKeyTimestampKey, types::Event,
    },
    serializer::indexed_type::{range::IndexedKeyRange, traits::IndexedKey},
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

pub async fn test_state_key_timestamp_index(store: IndexeddbEventCacheStore) {
    let room_id = *DEFAULT_TEST_ROOM_ID;
    let f = EventFactory::new().room(room_id).sender(user_id!("@alice:localhost"));
    let state_key = user_id!("@bob:localhost");

    let old_message_id = event_id!("$old_message");
    let new_message_id = event_id!("$new_message");
    let empty_state_key_id = event_id!("$empty_state_key");
    let state_key_id = event_id!("$state_key");

    for event in [
        f.text_msg("old").event_id(old_message_id).server_ts(1_000).into_event(),
        f.text_msg("new").event_id(new_message_id).server_ts(5_000).into_event(),
        f.room_topic("topic")
            .state_key("")
            .event_id(empty_state_key_id)
            .server_ts(1_000)
            .into_event(),
        f.member(state_key).event_id(state_key_id).server_ts(1_000).into_event(),
    ] {
        store.save_event(room_id, event).await.unwrap();
    }

    // Find the IDs of the events in `room_id` with the given `state_key` and
    // a timestamp strictly lower than `cutoff_ms`, using the index.
    let find = async |room_id: &RoomId, state_key: Option<&str>, cutoff_ms: u64| {
        let transaction = store.transaction(&[keys::EVENTS], IdbTransactionMode::Readonly).unwrap();
        let serializer = transaction.serializer().inner();
        let lower = IndexedEventStateKeyTimestampKey::encode(
            (room_id, state_key, MilliSecondsSinceUnixEpoch(0u32.into())),
            serializer,
        );
        let upper = IndexedEventStateKeyTimestampKey::encode(
            (room_id, state_key, MilliSecondsSinceUnixEpoch((cutoff_ms - 1).try_into().unwrap())),
            serializer,
        );
        let mut event_ids: Vec<OwnedEventId> = transaction
            .get_items_by_key::<Event, IndexedEventStateKeyTimestampKey>(IndexedKeyRange::Bound(
                lower, upper,
            ))
            .await
            .unwrap()
            .iter()
            .map(|event| event.event_id().unwrap().to_owned())
            .collect();
        event_ids.sort();
        event_ids
    };
    let ids = |ids: &[&EventId]| ids.iter().map(|id| (*id).to_owned()).collect::<Vec<_>>();

    // Non-state events.
    assert_eq!(find(room_id, None, 2_000).await, ids(&[old_message_id]));
    let mut all_messages = ids(&[old_message_id, new_message_id]);
    all_messages.sort();
    assert_eq!(find(room_id, None, 10_000).await, all_messages);

    // State events with an empty `state_key`.
    assert_eq!(find(room_id, Some(""), 2_000).await, ids(&[empty_state_key_id]));

    // State events with a non-empty `state_key`.
    assert_eq!(find(room_id, Some(state_key.as_str()), 2_000).await, ids(&[state_key_id]));
    assert_eq!(find(room_id, Some(state_key.as_str()), 1_000).await, ids(&[]));
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
            async fn test_state_key_timestamp_index() {
                let store = get_event_cache_store().await.expect("Failed to get event cache store");
                $crate::event_cache_store::integration_tests::test_state_key_timestamp_index(store)
                    .await
            }
        }
    };
}
