// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::{fmt, fs, path::Path, sync::Arc};

use ruma::{OwnedEventId, OwnedRoomId, RoomId, events::AnySyncMessageLikeEvent};
use tantivy::{
    Index, IndexReader, TantivyDocument,
    collector::TopDocs,
    directory::{MmapDirectory, error::OpenDirectoryError},
    query::QueryParser,
    schema::Value,
};
use tracing::error;

use crate::{
    OpStamp, TANTIVY_INDEX_MEMORY_BUDGET,
    error::IndexError,
    schema::{MatrixSearchIndexSchema, RoomMessageSchema},
    writer::SearchIndexWriter,
};

/// A struct to represent the operations on a [`RoomIndex`]
pub(crate) enum RoomIndexOperation {
    Add(TantivyDocument),
}

/// A struct that holds all data pertaining to a particular room's
/// message index.
pub struct RoomIndex {
    schema: RoomMessageSchema,
    writer: SearchIndexWriter,
    reader: IndexReader,
    query_parser: QueryParser,
    room_id: OwnedRoomId,
}

impl fmt::Debug for RoomIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomIndex")
            .field("schema", &self.schema)
            .field("room_id", &self.room_id)
            .finish()
    }
}

impl RoomIndex {
    fn new_with(
        index: Index,
        schema: RoomMessageSchema,
        room_id: &RoomId,
    ) -> Result<RoomIndex, IndexError> {
        let writer = index.writer(TANTIVY_INDEX_MEMORY_BUDGET)?;
        let reader = index.reader_builder().try_into()?;

        let query_parser = QueryParser::for_index(&index, schema.default_search_fields());
        Ok(Self {
            schema,
            writer: writer.into(),
            reader,
            query_parser,
            room_id: room_id.to_owned(),
        })
    }

    /// Create new [`RoomIndex`] which stores the index in path/room_id
    pub fn new(path: &Path, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let path = path.join(room_id.as_str());
        let schema = RoomMessageSchema::new();
        fs::create_dir_all(path.clone())?;
        let index = Index::create_in_dir(path, schema.as_tantivy_schema())?;
        RoomIndex::new_with(index, schema, room_id)
    }

    /// Create new [`RoomIndex`] which stores the index in memory.
    /// Intended for testing.
    pub fn new_in_memory(room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let schema = RoomMessageSchema::new();
        let index = Index::create_in_ram(schema.as_tantivy_schema());
        RoomIndex::new_with(index, schema, room_id)
    }

    /// Open index at path/room_id if it exists else
    /// create new [`RoomIndex`] which stores the index in path/room_id
    pub fn open_or_create(path: &Path, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let path = path.join(room_id.as_str());
        let mmap_dir = match MmapDirectory::open(path) {
            Ok(dir) => Ok(dir),
            Err(err) => match err {
                OpenDirectoryError::DoesNotExist(path) => {
                    fs::create_dir_all(path.clone()).map_err(|err| {
                        OpenDirectoryError::IoError {
                            io_error: Arc::new(err),
                            directory_path: path.to_path_buf(),
                        }
                    })?;
                    MmapDirectory::open(path)
                }
                _ => Err(err),
            },
        }?;
        let schema = RoomMessageSchema::new();
        let index = Index::open_or_create(mmap_dir, schema.as_tantivy_schema())?;
        RoomIndex::new_with(index, schema, room_id)
    }

    /// Open index at path/room_id. Fails if it doesn't exist.
    pub fn open(path: &Path, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let path = path.join(room_id.as_str());
        let index_path = MmapDirectory::open(path)?;
        let index = Index::open(index_path)?;
        let schema: RoomMessageSchema = index.schema().try_into()?;
        RoomIndex::new_with(index, schema, room_id)
    }

    /// Handle [`AnySyncMessageLikeEvent`]
    ///
    /// This which will add/remove/edit an event in the index based on the
    /// event type.
    pub fn handle_event(&mut self, event: AnySyncMessageLikeEvent) -> Result<(), IndexError> {
        match self.schema.handle_event(event)? {
            RoomIndexOperation::Add(document) => self.writer.add_document(document)?,
        };
        Ok(())
    }

    /// Commit added events to [`RoomIndex`]
    pub fn commit(&mut self) -> Result<OpStamp, IndexError> {
        let last_commit_opstamp = self.writer.commit()?; // TODO: This is blocking. Handle it.
        Ok(last_commit_opstamp)
    }

    /// Commit added events to [`RoomIndex`] and
    /// update searchers so that they reflect the state of the last
    /// `.commit()`.
    ///
    /// Every commit should be rapidly reflected on your `IndexReader` and you
    /// should not need to call `reload()` at all.
    ///
    /// This automatic reload can take 10s of milliseconds to kick in however,
    /// and in unit tests it can be nice to deterministically force the
    /// reload of searchers.
    pub fn commit_and_reload(&mut self) -> Result<OpStamp, IndexError> {
        let last_commit_opstamp = self.writer.commit()?; // TODO: This is blocking. Handle it.
        self.reader.reload()?;
        Ok(last_commit_opstamp)
    }

    /// Search the [`RoomIndex`] for some query. Returns a list of
    /// results with a maximum given length.
    pub fn search(
        &self,
        query: &str,
        max_number_of_results: usize,
    ) -> Result<Vec<OwnedEventId>, IndexError> {
        let query = self.query_parser.parse_query(query)?;
        let searcher = self.reader.searcher();

        let results = searcher.search(&query, &TopDocs::with_limit(max_number_of_results))?;
        let mut ret: Vec<OwnedEventId> = Vec::new();
        let pk = self.schema.primary_key();

        for (_score, doc_address) in results {
            let retrieved_doc: TantivyDocument = searcher.doc(doc_address)?;
            match retrieved_doc.get_first(pk).and_then(|maybe_value| maybe_value.as_str()) {
                Some(value) => match OwnedEventId::try_from(value) {
                    Ok(event_id) => ret.push(event_id),
                    Err(err) => error!("error while parsing event_id from search result: {err:?}"),
                },
                _ => error!("unexpected value type while searching documents"),
            }
        }

        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, error::Error};

    use matrix_sdk_test::event_factory::EventFactory;
    use ruma::{event_id, room_id, user_id};

    use crate::index::RoomIndex;

    #[test]
    fn test_make_index_in_memory() {
        let room_id = room_id!("!room_id:localhost");
        let index = RoomIndex::new_in_memory(room_id);

        index.expect("failed to make index in ram: {index:?}");
    }

    #[test]
    fn test_handle_event() {
        let room_id = room_id!("!room_id:localhost");
        let mut index =
            RoomIndex::new_in_memory(room_id).expect("failed to make index in ram: {index:?}");

        let event = EventFactory::new()
            .text_msg("event message")
            .event_id(event_id!("$event_id:localhost"))
            .room(room_id)
            .sender(user_id!("@user_id:localhost"))
            .into_any_sync_message_like_event();

        index.handle_event(event).expect("failed to add event: {res:?}");
    }

    #[test]
    fn test_search_populated_index() -> Result<(), Box<dyn Error>> {
        let room_id = room_id!("!room_id:localhost");
        let mut index =
            RoomIndex::new_in_memory(room_id).expect("failed to make index in ram: {index:?}");

        let event_id_1 = event_id!("$event_id_1:localhost");
        let event_id_2 = event_id!("$event_id_2:localhost");
        let event_id_3 = event_id!("$event_id_3:localhost");

        index.handle_event(
            EventFactory::new()
                .text_msg("This is a sentence")
                .event_id(event_id_1)
                .room(room_id)
                .sender(user_id!("@user_id:localhost"))
                .into_any_sync_message_like_event(),
        )?;

        index.handle_event(
            EventFactory::new()
                .text_msg("All new words")
                .event_id(event_id_2)
                .room(room_id)
                .sender(user_id!("@user_id:localhost"))
                .into_any_sync_message_like_event(),
        )?;

        index.handle_event(
            EventFactory::new()
                .text_msg("A similar sentence")
                .event_id(event_id_3)
                .room(room_id)
                .sender(user_id!("@user_id:localhost"))
                .into_any_sync_message_like_event(),
        )?;

        index.commit_and_reload()?;

        let result = index.search("sentence", 10).expect("search failed with: {result:?}");
        let result: HashSet<_> = result.iter().collect();

        let true_value = [event_id_1.to_owned(), event_id_3.to_owned()];
        let true_value: HashSet<_> = true_value.iter().collect();

        assert_eq!(result, true_value, "search result not correct: {result:?}");

        Ok(())
    }

    #[test]
    fn test_search_empty_index() -> Result<(), Box<dyn Error>> {
        let room_id = room_id!("!room_id:localhost");
        let mut index =
            RoomIndex::new_in_memory(room_id).expect("failed to make index in ram: {index:?}");

        index.commit_and_reload()?;

        let result = index.search("sentence", 10).expect("search failed with: {result:?}");

        assert!(result.is_empty(), "search result not empty: {result:?}");

        Ok(())
    }
}
