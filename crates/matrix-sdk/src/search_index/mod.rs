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

//! The search index is an abstraction layer in the matrix-sdk for the
//! matrix-sdk-search crate. It provides a [`SearchIndex`] which wraps
//! multiple [`RoomIndex`].

use std::{collections::hash_map::HashMap, path::PathBuf, sync::Arc};

use futures_util::future::join_all;
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use matrix_sdk_search::{
    error::IndexError,
    index::{RoomIndex, RoomIndexOperation, builder::RoomIndexBuilder},
};
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, RoomId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        room::{
            message::{OriginalSyncRoomMessageEvent, Relation, SyncRoomMessageEvent},
            redaction::SyncRoomRedactionEvent,
        },
    },
    room_version_rules::RedactionRules,
};
use tokio::sync::{Mutex, MutexGuard};
use tracing::{debug, warn};

use crate::event_cache::RoomEventCache;

type Password = String;

/// Type of location to store [`RoomIndex`]
#[derive(Clone, Debug)]
pub enum SearchIndexStoreKind {
    /// Store unencrypted in file system folder
    UnencryptedDirectory(PathBuf),
    /// Store encrypted in file system folder
    EncryptedDirectory(PathBuf, Password),
    /// Store in memory
    InMemory,
}

/// Object that handles inteeraction with [`RoomIndex`]'s for search
#[derive(Clone, Debug)]
pub struct SearchIndex {
    /// HashMap that links each joined room to its RoomIndex
    room_indexes: Arc<Mutex<HashMap<OwnedRoomId, RoomIndex>>>,

    /// Base directory that stores the directories for each RoomIndex
    search_index_store_kind: SearchIndexStoreKind,
}

impl SearchIndex {
    /// Create a new [`SearchIndex`]
    pub fn new(
        room_indexes: Arc<Mutex<HashMap<OwnedRoomId, RoomIndex>>>,
        search_index_store_kind: SearchIndexStoreKind,
    ) -> Self {
        Self { room_indexes, search_index_store_kind }
    }

    /// Acquire [`SearchIndexGuard`] for this [`SearchIndex`].
    pub async fn lock(&self) -> SearchIndexGuard<'_> {
        SearchIndexGuard {
            index_map: self.room_indexes.lock().await,
            search_index_store_kind: &self.search_index_store_kind,
        }
    }
}

/// Object that represents an acquired [`SearchIndex`].
#[derive(Debug)]
pub struct SearchIndexGuard<'a> {
    /// Guard around the [`RoomIndex`] map
    index_map: MutexGuard<'a, HashMap<OwnedRoomId, RoomIndex>>,

    /// Base directory that stores the directories for each RoomIndex
    search_index_store_kind: &'a SearchIndexStoreKind,
}

impl SearchIndexGuard<'_> {
    fn create_index(&self, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let index = match self.search_index_store_kind {
            SearchIndexStoreKind::UnencryptedDirectory(path) => {
                RoomIndexBuilder::new_on_disk(path.to_path_buf(), room_id).unencrypted().build()?
            }
            SearchIndexStoreKind::EncryptedDirectory(path, password) => {
                RoomIndexBuilder::new_on_disk(path.to_path_buf(), room_id)
                    .encrypted(password)
                    .build()?
            }
            SearchIndexStoreKind::InMemory => RoomIndexBuilder::new_in_memory(room_id).build(),
        };
        Ok(index)
    }

    /// Handle a [`RoomIndexOperation`] in the [`RoomIndex`] of a given
    /// [`RoomId`]
    ///
    /// This which will add/remove/edit an event in the index based on the
    /// event type.
    ///
    /// Prefer [`SearchIndexGuard::bulk_execute`] for multiple operations.
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

        index.execute(operation)
    }

    /// Handle a [`RoomIndexOperation`] in the [`RoomIndex`] of a given
    /// [`RoomId`]
    ///
    /// This which will add/remove/edit an event in the index based on the
    /// event type.
    pub(crate) fn bulk_execute(
        &mut self,
        operations: Vec<RoomIndexOperation>,
        room_id: &RoomId,
    ) -> Result<(), IndexError> {
        if !self.index_map.contains_key(room_id) {
            let index = self.create_index(room_id)?;
            self.index_map.insert(room_id.to_owned(), index);
        }

        let index = self.index_map.get_mut(room_id).expect("index should exist");

        index.bulk_execute(operations)
    }

    /// Search a [`Room`]'s index for the query and return at most
    /// max_number_of_results results.
    pub(crate) fn search(
        &mut self,
        query: &str,
        max_number_of_results: usize,
        pagination_offset: Option<usize>,
        room_id: &RoomId,
    ) -> Result<Vec<OwnedEventId>, IndexError> {
        if !self.index_map.contains_key(room_id) {
            let index = self.create_index(room_id)?;
            self.index_map.insert(room_id.to_owned(), index);
        }

        let index = self.index_map.get_mut(room_id).expect("index should exist");

        index.search(query, max_number_of_results, pagination_offset)
    }

    /// Given a [`TimelineEvent`] this function will derive a
    /// [`RoomIndexOperation`], if it should be handled, and execute it;
    /// returning the result.
    ///
    /// Prefer [`SearchIndexGuard::bulk_handle_timeline_event`] for multiple
    /// events.
    pub async fn handle_timeline_event(
        &mut self,
        event: TimelineEvent,
        room_cache: &RoomEventCache,
        room_id: &RoomId,
        redaction_rules: &RedactionRules,
    ) -> Result<(), IndexError> {
        if let Some(index_operation) =
            parse_timeline_event(room_cache, event, redaction_rules).await
        {
            self.execute(index_operation, room_id)
        } else {
            Ok(())
        }
    }

    /// Run [`SearchIndexGuard::handle_timeline_event`] for multiple
    /// [`TimelineEvent`].
    pub async fn bulk_handle_timeline_event<T>(
        &mut self,
        events: T,
        room_cache: &RoomEventCache,
        room_id: &RoomId,
        redaction_rules: &RedactionRules,
    ) -> Result<(), IndexError>
    where
        T: Iterator<Item = TimelineEvent>,
    {
        let futures = events.map(|ev| parse_timeline_event(room_cache, ev, redaction_rules));

        let operations: Vec<_> = join_all(futures).await.into_iter().flatten().collect();

        self.bulk_execute(operations, room_id)
    }
}

/// Given an event id this function returns the most recent edit on said event
/// or the event itself if there are no edits.
async fn get_most_recent_edit(
    cache: &RoomEventCache,
    original: &EventId,
) -> Option<OriginalSyncRoomMessageEvent> {
    use ruma::events::{AnySyncTimelineEvent, relation::RelationType};

    let Ok(Some((original_ev, related))) =
        cache.find_event_with_relations(original, Some(vec![RelationType::Replacement])).await
    else {
        debug!("Couldn't find relations for {}", original);
        return None;
    };

    match related.last().unwrap_or(&original_ev).raw().deserialize() {
        Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(latest))) => {
            latest.as_original().cloned()
        }
        _ => None,
    }
}

/// If the given [`OriginalSyncRoomMessageEvent`] is an edit we make an
/// [`RoomIndexOperation::Edit`] with the new most recent version of the
/// original.
async fn handle_possible_edit(
    event: &OriginalSyncRoomMessageEvent,
    cache: &RoomEventCache,
) -> Option<RoomIndexOperation> {
    if let Some(Relation::Replacement(replacement_data)) = &event.content.relates_to {
        if let Some(recent) = get_most_recent_edit(cache, &replacement_data.event_id).await {
            return Some(RoomIndexOperation::Edit(replacement_data.event_id.clone(), recent));
        } else {
            return Some(RoomIndexOperation::Noop);
        }
    }
    None
}

/// Return a [`RoomIndexOperation::Edit`] or [`RoomIndexOperation::Add`]
/// depending on the message.
async fn handle_room_message(
    event: SyncRoomMessageEvent,
    cache: &RoomEventCache,
) -> Option<RoomIndexOperation> {
    if let Some(event) = event.as_original() {
        return handle_possible_edit(event, cache).await.or(get_most_recent_edit(
            cache,
            &event.event_id,
        )
        .await
        .map(RoomIndexOperation::Add));
    }
    None
}

/// Return a [`RoomIndexOperation::Edit`] or [`RoomIndexOperation::Remove`]
/// depending on the message.
async fn handle_room_redaction(
    event: SyncRoomRedactionEvent,
    cache: &RoomEventCache,
    rules: &RedactionRules,
) -> Option<RoomIndexOperation> {
    if let Some(redacted_event_id) = event.redacts(rules)
        && let Ok(Some(redacted_event)) = cache.find_event(redacted_event_id).await
        && let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            redacted_event,
        ))) = redacted_event.raw().deserialize()
        && let Some(redacted_event) = redacted_event.as_original()
    {
        return handle_possible_edit(redacted_event, cache)
            .await
            .or(Some(RoomIndexOperation::Remove(redacted_event.event_id.clone())));
    }
    None
}

/// Prepare a [`TimelineEvent`] into a [`RoomIndexOperation`] for search
/// indexing.
async fn parse_timeline_event(
    cache: &RoomEventCache,
    event: TimelineEvent,
    redaction_rules: &RedactionRules,
) -> Option<RoomIndexOperation> {
    use ruma::events::AnySyncTimelineEvent;

    if event.kind.is_utd() {
        return None;
    }

    match event.raw().deserialize() {
        Ok(event) => match event {
            AnySyncTimelineEvent::MessageLike(event) => match event {
                AnySyncMessageLikeEvent::RoomMessage(event) => {
                    handle_room_message(event, cache).await
                }
                AnySyncMessageLikeEvent::RoomRedaction(event) => {
                    handle_room_redaction(event, cache, redaction_rules).await
                }
                _ => None,
            },
            AnySyncTimelineEvent::State(_) => None,
        },

        Err(e) => {
            warn!("failed to parse event: {e:?}");
            None
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
        let dummy = f.text_msg("dummy").event_id(dummy_id);

        let original = f.text_msg("This is a message").event_id(original_id);

        let edit1 = f
            .text_msg("* A new message")
            .edit(original_id, RoomMessageEventContentWithoutRelation::text_plain("A new message"))
            .event_id(edit1_id);

        let edit2 = f
            .text_msg("* An even newer message")
            .edit(
                original_id,
                RoomMessageEventContentWithoutRelation::text_plain("An even newer message"),
            )
            .event_id(edit2_id);

        let edit3 = f
            .text_msg("* The newest message")
            .edit(
                original_id,
                RoomMessageEventContentWithoutRelation::text_plain("The newest message"),
            )
            .event_id(edit3_id);

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(dummy)
                    .add_timeline_event(edit1)
                    .add_timeline_event(edit2),
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
