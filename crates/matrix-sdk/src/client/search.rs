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

use matrix_sdk_search::{error::IndexError, index::RoomIndex};
use ruma::{events::AnySyncMessageLikeEvent, OwnedEventId, OwnedRoomId, RoomId};
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
    pub(crate) fn handle_event(
        &mut self,
        event: AnySyncMessageLikeEvent,
        room_id: &RoomId,
    ) -> Result<(), IndexError> {
        if !self.index_map.contains_key(room_id) {
            let index = self.create_index(room_id)?;
            self.index_map.insert(room_id.to_owned(), index);
        }

        let index = self.index_map.get_mut(room_id).expect("index should exist");
        let result = index.handle_event(event);

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
        room_id: &RoomId,
    ) -> Option<Vec<OwnedEventId>> {
        if let Some(index) = self.index_map.get(room_id) {
            index
                .search(query, max_number_of_results)
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
