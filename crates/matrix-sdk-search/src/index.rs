#![forbid(missing_docs)]

use std::{fmt, path::Path};

use ruma::{events::AnyMessageLikeEvent, OwnedRoomId, RoomId};
use tantivy::{
    collector::TopDocs, directory::MmapDirectory, query::QueryParser, schema::OwnedValue, Index,
    IndexReader, TantivyDocument,
};

use crate::{
    error::IndexError, schema::RoomMessageSchema, util::TANTIVY_INDEX_MEMORY_BUDGET,
    writer::SearchIndexWriter, OpStamp,
};

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

    /// Create new [`RoomIndex`] which stores the index in <path>/<room_id>
    pub fn new(path: &Path, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let path = path.join(room_id.as_str());
        let schema = RoomMessageSchema::new();
        let index = Index::create_in_dir(path, schema.as_tantivy_schema())?;
        RoomIndex::new_with(index, schema, room_id)
    }

    /// Create new [`RoomIndex`] which stores the index in RAM.
    /// Intended for testing.
    pub fn new_in_ram(room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let schema = RoomMessageSchema::new();
        let index = Index::create_in_ram(schema.as_tantivy_schema());
        RoomIndex::new_with(index, schema, room_id)
    }

    /// Open index at <path>/<room_id> if it exists else
    /// create new [`RoomIndex`] which stores the index in <path>/<room_id>
    pub fn open_or_create(path: &Path, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let path = path.join(room_id.as_str());
        let mmap_dir = MmapDirectory::open(path)?;
        let schema = RoomMessageSchema::new();
        let index = Index::open_or_create(mmap_dir, schema.as_tantivy_schema())?;
        RoomIndex::new_with(index, schema, room_id)
    }

    /// Open index at <path>/<room_id>. Fails if it doesn't exist.
    pub fn open(path: &Path, room_id: &RoomId) -> Result<RoomIndex, IndexError> {
        let path = path.join(room_id.as_str());
        let index_path = MmapDirectory::open(path)?;
        let index = Index::open(index_path)?;
        let schema: RoomMessageSchema = index.schema().try_into()?;
        RoomIndex::new_with(index, schema, room_id)
    }

    /// Add [`AnyMessageLikeEvent`] to [`RoomIndex`]
    pub fn add_event(&mut self, event: AnyMessageLikeEvent) -> Result<OpStamp, IndexError> {
        let doc = self.schema.make_doc(event)?;
        self.writer.add_document(doc)?;
        let last_commit_opstamp = self.writer.commit()?;

        Ok(last_commit_opstamp)
    }

    /// Forces [`RoomIndex`] to commit changes.
    /// Nominally [`RoomIndex`] stores up changes and batch commits.
    /// However, for some circumstances (such as testing), it may be
    /// beneficial to know a commit occurred before moving on.
    pub fn force_commit(&mut self) -> Result<OpStamp, IndexError> {
        let opstamp = self.writer.force_commit()?;
        self.reader.reload()?;

        Ok(opstamp)
    }

    /// Search the [`RoomIndex`] for some query. Returns a list of
    /// results with a maximum given length.
    pub fn search(
        &self,
        query: &str,
        max_number_of_results: usize,
    ) -> Result<Vec<String>, IndexError> {
        let query = self.query_parser.parse_query(query)?;
        let searcher = self.reader.searcher();

        let results = searcher.search(&query, &TopDocs::with_limit(max_number_of_results))?;
        let mut ret: Vec<String> = Vec::new();
        let pk = self.schema.primary_key();

        for (_score, doc_address) in results {
            let retrieved_doc: TantivyDocument = searcher.doc(doc_address)?;
            for f in retrieved_doc.get_all(pk) {
                match f {
                    OwnedValue::Str(s) => ret.push(s.to_string()),
                    _ => println!("how"),
                };
            }
        }

        Ok(ret)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, error::Error};

    use ruma::{
        event_id,
        events::{message::MessageEventContent, AnyMessageLikeEvent},
        room_id,
    };

    use crate::{index::RoomIndex, testing::event_builder::NewEventBuilder};

    #[test]
    fn test_make_index_in_ram() {
        let room_id = room_id!("!room_id:localhost");
        let index = RoomIndex::new_in_ram(room_id);

        assert!(index.is_ok(), "failed to make index in ram: {:?}", index.unwrap_err());
    }

    #[test]
    fn test_add_event() {
        let room_id = room_id!("!room_id:localhost");
        let index = RoomIndex::new_in_ram(room_id);

        assert!(index.is_ok(), "failed to make index in ram: {:?}", index.unwrap_err());

        let mut index = index.unwrap();

        let event = NewEventBuilder::new()
            .content(MessageEventContent::plain("event message"))
            .event_id(event_id!("$event_id:localhost"))
            .build_as(AnyMessageLikeEvent::Message);

        let res = index.add_event(event);

        assert!(res.is_ok(), "failed to add event: {:?}", res.unwrap_err());
    }

    #[test]
    fn test_search_populated_index() -> Result<(), Box<dyn Error>> {
        let room_id = room_id!("!room_id:localhost");
        let index = RoomIndex::new_in_ram(room_id);

        assert!(index.is_ok(), "failed to make index in ram: {:?}", index.unwrap_err());

        let mut index = index.unwrap();

        index.add_event(
            NewEventBuilder::new()
                .content(MessageEventContent::plain("This is a sentence"))
                .event_id(event_id!("$event_id_1:localhost"))
                .build_as(AnyMessageLikeEvent::Message),
        )?;

        index.add_event(
            NewEventBuilder::new()
                .content(MessageEventContent::plain("All new words"))
                .event_id(event_id!("$event_id_2:localhost"))
                .build_as(AnyMessageLikeEvent::Message),
        )?;

        index.add_event(
            NewEventBuilder::new()
                .content(MessageEventContent::plain("A similar sentence"))
                .event_id(event_id!("$event_id_3:localhost"))
                .build_as(AnyMessageLikeEvent::Message),
        )?;

        index.force_commit()?;

        let result = index.search("sentence", 10);

        assert!(result.is_ok(), "search failed with: {:?}", result.unwrap_err());

        let result = result.unwrap();
        let result: HashSet<_> = result.iter().collect();
        let true_value =
            vec!["$event_id_1:localhost".to_string(), "$event_id_3:localhost".to_string()];
        let true_value: HashSet<_> = true_value.iter().collect();

        assert_eq!(result, true_value, "search result not correct: {result:?}");

        Ok(())
    }

    #[test]
    fn test_search_empty_index() -> Result<(), Box<dyn Error>> {
        let room_id = room_id!("!room_id:localhost");
        let index = RoomIndex::new_in_ram(room_id);

        assert!(index.is_ok(), "failed to make index in ram: {:?}", index.unwrap_err());

        let mut index = index.unwrap();

        index.force_commit()?;

        let result = index.search("sentence", 10);

        assert!(result.is_ok(), "search failed with: {:?}", result.unwrap_err());

        let result = result.unwrap();

        assert!(result.is_empty(), "search result not empty: {result:?}");

        Ok(())
    }
}
