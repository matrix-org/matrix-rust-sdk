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
// limitations under the License.

use std::{collections::hash_map::HashMap, path::PathBuf, sync::Arc};

use matrix_sdk_search::{
    error::IndexError,
    index::{RoomIndex, RoomIndexOperation},
};
use ruma::{OwnedEventId, OwnedRoomId, RoomId};
use tokio::sync::{Mutex, MutexGuard};
use tracing::{debug, error};

/// Type of location to store [`RoomIndex`]
#[derive(Clone, Debug)]
pub enum SearchIndexStoreKind {
    /// Store in file system folder
    Directory(PathBuf),
    /// Store in memory
    InMemory,
}

/// Object that handles inteeraction with [`RoomIndex`]'s for search
#[derive(Clone, Debug)]
pub(crate) struct SearchIndex {
    /// HashMap that links each joined room to its RoomIndex
    room_indexes: Arc<Mutex<HashMap<OwnedRoomId, RoomIndex>>>,

    /// Base directory that stores the directories for each RoomIndex
    search_index_store_kind: SearchIndexStoreKind,
}

impl SearchIndex {
    /// Create a new [`SearchIndexHandler`]
    pub fn new(
        room_indexes: Arc<Mutex<HashMap<OwnedRoomId, RoomIndex>>>,
        search_index_store_kind: SearchIndexStoreKind,
    ) -> Self {
        Self { room_indexes, search_index_store_kind }
    }

    pub async fn lock(&self) -> SearchIndexGuard<'_> {
        SearchIndexGuard {
            index_map: self.room_indexes.lock().await,
            search_index_store_kind: &self.search_index_store_kind,
        }
    }
}

pub(crate) struct SearchIndexGuard<'a> {
    /// Guard around the [`RoomIndex`] map
    index_map: MutexGuard<'a, HashMap<OwnedRoomId, RoomIndex>>,

    /// Base directory that stores the directories for each RoomIndex
    search_index_store_kind: &'a SearchIndexStoreKind,
}

impl SearchIndexGuard<'_> {
    fn create_index(&self, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let index = match self.search_index_store_kind {
            SearchIndexStoreKind::Directory(path) => RoomIndex::open_or_create(path, room_id)?,
            SearchIndexStoreKind::InMemory => RoomIndex::new_in_memory(room_id)?,
        };
        Ok(index)
    }

    /// Handle an [`AnySyncMessageLikeEvent`] in the [`RoomIndex`] of a given
    /// [`RoomId`]
    ///
    /// This which will add/remove/edit an event in the index based on the
    /// event type.
    pub(crate) fn execute(
        &mut self,
        operation: RoomIndexOperation,
        room_id: &RoomId,
    ) -> Result<(), IndexError> {
        if !self.index_map.contains_key(room_id) {
            let index = self.create_index(room_id)?;
            self.index_map.insert(room_id.to_owned(), index);
        }

        let index = self.index_map.get_mut(room_id).expect("index should exist");
        let result = index.execute(operation);

        match result {
            Ok(_) => {}
            Err(IndexError::CannotIndexRedactedMessage)
            | Err(IndexError::EmptyMessage)
            | Err(IndexError::MessageTypeNotSupported) => {
                debug!("failed to parse event for indexing: {result:?}")
            }
            Err(IndexError::TantivyError(err)) => {
                error!("failed to handle event in index: {err:?}")
            }
            Err(_) => error!("unexpected error during indexing: {result:?}"),
        }
        Ok(())
    }

    /// Search a [`Room`]'s index for the query and return at most
    /// max_number_of_results results.
    pub(crate) fn search(
        &self,
        query: &str,
        max_number_of_results: usize,
        pagination_offset: Option<usize>,
        room_id: &RoomId,
    ) -> Option<Vec<OwnedEventId>> {
        if let Some(index) = self.index_map.get(room_id) {
            index
                .search(query, max_number_of_results, pagination_offset)
                .inspect_err(|err| {
                    error!("error occurred while searching index: {err:?}");
                })
                .ok()
        } else {
            debug!("Tried to search in a room with no index");
            None
        }
    }

    /// Commit a [`Room`]'s [`RoomIndex`] and reload searchers
    pub(crate) fn commit_and_reload(&mut self, room_id: &RoomId) {
        if let Some(index) = self.index_map.get_mut(room_id) {
            let _ = index.commit_and_reload().inspect_err(|err| {
                error!("error occurred while committing: {err:?}");
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{
        event_id, events::room::message::RoomMessageEventContentWithoutRelation, room_id, user_id,
    };

    use crate::test_utils::mocks::MatrixMockServer;

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_sync_message_is_indexed() {
        let mock_server = MatrixMockServer::new().await;
        let client = mock_server.client_builder().build().await;

        client.event_cache().subscribe().unwrap();

        let room_id = room_id!("!room_id:localhost");
        let event_id = event_id!("$event_id:localost");
        let user_id = user_id!("@user_id:localost");

        let event_factory = EventFactory::new();
        let room = mock_server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                    event_factory
                        .text_msg("this is a sentence")
                        .event_id(event_id)
                        .sender(user_id)
                        .into_raw_sync(),
                ]),
            )
            .await;

        let response = room.search("this", 5, None).await.expect("search should have 1 result");

        assert_eq!(response.len(), 1, "unexpected numbers of responses: {response:?}");
        assert_eq!(response[0], event_id, "event id doesn't match: {response:?}");
    }

    #[cfg(feature = "experimental-search")]
    #[async_test]
    async fn test_search_index_edit_ordering() {
        let room_id = room_id!("!room_id:localhost");
        let dummy_id = event_id!("$dummy");
        let edit1_id = event_id!("$edit1");
        let edit2_id = event_id!("$edit2");
        let edit3_id = event_id!("$edit3");
        let original_id = event_id!("$original");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room = server.sync_joined_room(&client, room_id).await;

        let f = EventFactory::new().room(room_id).sender(user_id!("@user_id:localhost"));

        // Indexable dummy message required because RoomIndex is initialised lazily.
        let dummy = f.text_msg("dummy").event_id(dummy_id).into_raw();

        let original = f.text_msg("This is a message").event_id(original_id).into_raw();

        let edit1 = f
            .text_msg("* A new message")
            .edit(original_id, RoomMessageEventContentWithoutRelation::text_plain("A new message"))
            .event_id(edit1_id)
            .into_raw();

        let edit2 = f
            .text_msg("* An even newer message")
            .edit(
                original_id,
                RoomMessageEventContentWithoutRelation::text_plain("An even newer message"),
            )
            .event_id(edit2_id)
            .into_raw();

        let edit3 = f
            .text_msg("* The newest message")
            .edit(
                original_id,
                RoomMessageEventContentWithoutRelation::text_plain("The newest message"),
            )
            .event_id(edit3_id)
            .into_raw();

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(dummy.clone())
                    .add_timeline_event(edit1.clone())
                    .add_timeline_event(edit2.clone()),
            )
            .await;

        let results = room.search("message", 3, None).await.unwrap();

        assert_eq!(results.len(), 0, "Search should return 0 results, got {results:?}");

        // Adding the original after some pending edits should add the latest edit
        // instead of the original.
        server
            .sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(original))
            .await;

        let results = room.search("message", 3, None).await.unwrap();

        assert_eq!(results.len(), 1, "Search should return 1 result, got {results:?}");
        assert_eq!(results[0], edit2_id, "Search should return latest edit, got {:?}", results[0]);

        // Editing the original after it exists and there has been another edit should
        // delete the previous edits and add this one
        server.sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(edit3)).await;

        let results = room.search("message", 3, None).await.unwrap();

        assert_eq!(results.len(), 1, "Search should return 1 result, got {results:?}");
        assert_eq!(results[0], edit3_id, "Search should return latest edit, got {:?}", results[0]);
    }
}
