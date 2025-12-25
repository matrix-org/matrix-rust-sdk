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

use std::ops::Deref;

use indexed_db_futures::transaction as inner;
use matrix_sdk_base::{
    event_cache::{Event as RawEvent, Gap as RawGap},
    linked_chunk::{ChunkContent, ChunkIdentifier, LinkedChunkId, RawChunk},
};
use ruma::{EventId, RoomId, events::relation::RelationType};
use serde::{Serialize, de::DeserializeOwned};

use crate::{
    error::AsyncErrorDeps,
    event_cache_store::{
        serializer::indexed_types::{
            IndexedChunk, IndexedChunkIdKey, IndexedEvent, IndexedEventIdKey,
            IndexedEventPositionKey, IndexedEventRelationKey, IndexedEventRoomKey, IndexedGapIdKey,
            IndexedLease, IndexedLeaseIdKey, IndexedNextChunkIdKey,
        },
        types::{Chunk, ChunkType, Event, Gap, Lease, Position},
    },
    serializer::indexed_type::{
        IndexedTypeSerializer,
        range::IndexedKeyRange,
        traits::{Indexed, IndexedPrefixKeyBounds, IndexedPrefixKeyComponentBounds},
    },
    transaction::{Transaction, TransactionError},
};

/// Represents an IndexedDB transaction, but provides a convenient interface for
/// performing operations relevant to the IndexedDB implementation of
/// [`EventCacheStore`](matrix_sdk_base::event_cache::store::EventCacheStore).
pub struct IndexeddbEventCacheStoreTransaction<'a> {
    transaction: Transaction<'a>,
}

impl<'a> Deref for IndexeddbEventCacheStoreTransaction<'a> {
    type Target = Transaction<'a>;

    fn deref(&self) -> &Self::Target {
        &self.transaction
    }
}

impl<'a> IndexeddbEventCacheStoreTransaction<'a> {
    pub fn new(transaction: inner::Transaction<'a>, serializer: &'a IndexedTypeSerializer) -> Self {
        Self { transaction: Transaction::new(transaction, serializer) }
    }

    /// Commit all operations tracked in this transaction to IndexedDB.
    pub async fn commit(self) -> Result<(), TransactionError> {
        self.transaction.commit().await
    }

    /// Query IndexedDB for all items matching the given linked chunk id by key
    /// `K`
    pub async fn get_items_by_linked_chunk_id<'b, T, K>(
        &self,
        linked_chunk_id: LinkedChunkId<'b>,
    ) -> Result<Vec<T>, TransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedPrefixKeyBounds<T, LinkedChunkId<'b>> + Serialize,
    {
        self.get_items_by_key::<T, K>(IndexedKeyRange::all_with_prefix(
            linked_chunk_id,
            self.serializer().inner(),
        ))
        .await
    }

    /// Query IndexedDB for all items of type `T` by key `K` in the given room
    pub async fn get_items_in_room<'b, T, K>(
        &self,
        room_id: &'b RoomId,
    ) -> Result<Vec<T>, TransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedPrefixKeyBounds<T, &'b RoomId> + Serialize,
    {
        self.get_items_by_key::<T, K>(IndexedKeyRange::all_with_prefix(
            room_id,
            self.serializer().inner(),
        ))
        .await
    }

    /// Query IndexedDB for the number of items matching the given linked chunk
    /// id.
    pub async fn get_items_count_by_linked_chunk_id<'b, T, K>(
        &self,
        linked_chunk_id: LinkedChunkId<'b>,
    ) -> Result<usize, TransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedPrefixKeyBounds<T, LinkedChunkId<'b>> + Serialize,
    {
        self.get_items_count_by_key::<T, K>(IndexedKeyRange::all_with_prefix(
            linked_chunk_id,
            self.serializer().inner(),
        ))
        .await
    }

    /// Delete all items of type `T` by key `K` associated with the given linked
    /// chunk id from IndexedDB
    pub async fn delete_items_by_linked_chunk_id<'b, T, K>(
        &self,
        linked_chunk_id: LinkedChunkId<'b>,
    ) -> Result<(), TransactionError>
    where
        T: Indexed,
        K: IndexedPrefixKeyBounds<T, LinkedChunkId<'b>> + Serialize,
    {
        self.delete_items_by_key::<T, K>(IndexedKeyRange::all_with_prefix(
            linked_chunk_id,
            self.serializer().inner(),
        ))
        .await
    }

    /// Query IndexedDB for the lease that matches the given key `id`. If more
    /// than one lease is found, an error is returned.
    pub async fn get_lease_by_id(&self, id: &str) -> Result<Option<Lease>, TransactionError> {
        self.get_item_by_key_components::<Lease, IndexedLeaseIdKey>(id).await
    }

    /// Puts a lease into IndexedDB. If an event with the same key already
    /// exists, it will be overwritten. When the item is successfully put, the
    /// function returns the intermediary type [`IndexedLease`] in case
    /// inspection is needed.
    pub async fn put_lease(&self, lease: &Lease) -> Result<IndexedLease, TransactionError> {
        self.put_item(lease).await
    }

    /// Query IndexedDB for chunks that match the given chunk identifier and the
    /// given linked chunk id. If more than one item is found, an error is
    /// returned.
    pub async fn get_chunk_by_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        chunk_id: ChunkIdentifier,
    ) -> Result<Option<Chunk>, TransactionError> {
        self.get_item_by_key_components::<Chunk, IndexedChunkIdKey>((linked_chunk_id, chunk_id))
            .await
    }

    /// Query IndexedDB for chunks such that the next chunk matches the given
    /// chunk identifier and the given linked chunk id. If more than one item is
    /// found, an error is returned.
    pub async fn get_chunk_by_next_chunk_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        next_chunk_id: Option<ChunkIdentifier>,
    ) -> Result<Option<Chunk>, TransactionError> {
        self.get_item_by_key_components::<Chunk, IndexedNextChunkIdKey>((
            linked_chunk_id,
            next_chunk_id,
        ))
        .await
    }

    /// Query IndexedDB for all chunks matching the given linked chunk id
    pub async fn get_chunks_by_linked_chunk_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<Chunk>, TransactionError> {
        self.get_items_by_linked_chunk_id::<Chunk, IndexedChunkIdKey>(linked_chunk_id).await
    }

    /// Query IndexedDB for the number of chunks matching the given linked chunk
    /// id.
    pub async fn get_chunks_count_by_linked_chunk_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<usize, TransactionError> {
        self.get_items_count_by_linked_chunk_id::<Chunk, IndexedChunkIdKey>(linked_chunk_id).await
    }

    /// Query IndexedDB for the chunk with the maximum key matching the given
    /// linked chunk id.
    pub async fn get_max_chunk_by_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Option<Chunk>, TransactionError> {
        let range = IndexedKeyRange::all_with_prefix::<Chunk, _>(
            linked_chunk_id,
            self.serializer().inner(),
        );
        self.get_max_item_by_key::<Chunk, IndexedChunkIdKey>(range).await
    }

    /// Query IndexedDB for given chunk matching the given linked chunk id and
    /// additionally query for events or gap, depending on chunk type, in
    /// order to construct the full chunk.
    pub async fn load_chunk_by_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        chunk_id: ChunkIdentifier,
    ) -> Result<Option<RawChunk<RawEvent, RawGap>>, TransactionError> {
        if let Some(chunk) = self.get_chunk_by_id(linked_chunk_id, chunk_id).await? {
            let content = match chunk.chunk_type {
                ChunkType::Event => {
                    let events = self
                        .get_events_by_chunk(
                            linked_chunk_id,
                            ChunkIdentifier::new(chunk.identifier),
                        )
                        .await?
                        .into_iter()
                        .map(RawEvent::from)
                        .collect();
                    ChunkContent::Items(events)
                }
                ChunkType::Gap => {
                    let gap = self
                        .get_gap_by_id(linked_chunk_id, ChunkIdentifier::new(chunk.identifier))
                        .await?
                        .ok_or(TransactionError::ItemNotFound)?;
                    ChunkContent::Gap(RawGap { prev_token: gap.prev_token })
                }
            };
            return Ok(Some(RawChunk {
                identifier: ChunkIdentifier::new(chunk.identifier),
                content,
                previous: chunk.previous.map(ChunkIdentifier::new),
                next: chunk.next.map(ChunkIdentifier::new),
            }));
        }
        Ok(None)
    }

    /// Add a chunk and ensure that the next and previous
    /// chunks are properly linked to the chunk being added. If a chunk with
    /// the same identifier already exists, the given chunk will be
    /// rejected. When the item is successfully added, the
    /// function returns the intermediary type [`IndexedChunk`] in case
    /// inspection is needed.
    pub async fn add_chunk(&self, chunk: &Chunk) -> Result<IndexedChunk, TransactionError> {
        let indexed = self.add_item(chunk).await?;
        if let Some(previous) = chunk.previous {
            let previous_identifier = ChunkIdentifier::new(previous);
            if let Some(mut previous_chunk) =
                self.get_chunk_by_id(chunk.linked_chunk_id.as_ref(), previous_identifier).await?
            {
                previous_chunk.next = Some(chunk.identifier);
                self.put_item(&previous_chunk).await?;
            }
        }
        if let Some(next) = chunk.next {
            let next_identifier = ChunkIdentifier::new(next);
            if let Some(mut next_chunk) =
                self.get_chunk_by_id(chunk.linked_chunk_id.as_ref(), next_identifier).await?
            {
                next_chunk.previous = Some(chunk.identifier);
                self.put_item(&next_chunk).await?;
            }
        }
        Ok(indexed)
    }

    /// Delete chunk that matches the given id and the given linked chunk id and
    /// ensure that the next and previous chunk are updated to link to one
    /// another. Additionally, ensure that events and gaps in the given
    /// chunk are also deleted.
    pub async fn delete_chunk_by_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        chunk_id: ChunkIdentifier,
    ) -> Result<(), TransactionError> {
        if let Some(chunk) = self.get_chunk_by_id(linked_chunk_id, chunk_id).await? {
            if let Some(previous) = chunk.previous {
                let previous_identifier = ChunkIdentifier::new(previous);
                if let Some(mut previous_chunk) =
                    self.get_chunk_by_id(linked_chunk_id, previous_identifier).await?
                {
                    previous_chunk.next = chunk.next;
                    self.put_item(&previous_chunk).await?;
                }
            }
            if let Some(next) = chunk.next {
                let next_identifier = ChunkIdentifier::new(next);
                if let Some(mut next_chunk) =
                    self.get_chunk_by_id(linked_chunk_id, next_identifier).await?
                {
                    next_chunk.previous = chunk.previous;
                    self.put_item(&next_chunk).await?;
                }
            }
            self.delete_item_by_key::<Chunk, IndexedChunkIdKey>((linked_chunk_id, chunk_id))
                .await?;
            match chunk.chunk_type {
                ChunkType::Event => {
                    self.delete_events_by_chunk(linked_chunk_id, chunk_id).await?;
                }
                ChunkType::Gap => {
                    self.delete_gap_by_id(linked_chunk_id, chunk_id).await?;
                }
            }
        }
        Ok(())
    }

    /// Delete all chunks associated with the given linked chunk id
    pub async fn delete_chunks_by_linked_chunk_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(), TransactionError> {
        self.delete_items_by_linked_chunk_id::<Chunk, IndexedChunkIdKey>(linked_chunk_id).await
    }

    /// Query IndexedDB for events that match the given event id and the given
    /// linked chunk id. If more than one item is found, an error is returned.
    pub async fn get_event_by_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        event_id: &EventId,
    ) -> Result<Option<Event>, TransactionError> {
        let key = self.serializer().encode_key((linked_chunk_id, event_id));
        self.get_item_by_key::<Event, IndexedEventIdKey>(key).await
    }

    /// Query IndexedDB for events that match the given event id in the given
    /// room. If more than one item is found, an error is returned.
    pub async fn get_event_by_room(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, TransactionError> {
        let key = self.serializer().encode_key((room_id, event_id));
        self.get_item_by_key::<Event, IndexedEventRoomKey>(key).await
    }

    /// Query IndexedDB for events that are in the given
    /// room.
    pub async fn get_room_events(&self, room_id: &RoomId) -> Result<Vec<Event>, TransactionError> {
        self.get_items_in_room::<Event, IndexedEventRoomKey>(room_id).await
    }

    /// Query IndexedDB for events in the given chunk matching the given linked
    /// chunk id.
    pub async fn get_events_by_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        chunk_id: ChunkIdentifier,
    ) -> Result<Vec<Event>, TransactionError> {
        let range = IndexedKeyRange::all_with_prefix(
            (linked_chunk_id, chunk_id),
            self.serializer().inner(),
        );
        self.get_items_by_key::<Event, IndexedEventPositionKey>(range).await
    }

    /// Query IndexedDB for number of events in the given chunk matching the
    /// given linked chunk id.
    pub async fn get_events_count_by_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        chunk_id: ChunkIdentifier,
    ) -> Result<usize, TransactionError> {
        let range = IndexedKeyRange::all_with_prefix(
            (linked_chunk_id, chunk_id),
            self.serializer().inner(),
        );
        self.get_items_count_by_key::<Event, IndexedEventPositionKey>(range).await
    }

    /// Query IndexedDB for events that match the given relation range in the
    /// given room.
    pub async fn get_events_by_relation(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<(&EventId, &RelationType)>>,
    ) -> Result<Vec<Event>, TransactionError> {
        let range = range
            .into()
            .map(|(event_id, relation_type)| (room_id, event_id, relation_type))
            .encoded(self.serializer().inner());
        self.get_items_by_key::<Event, IndexedEventRelationKey>(range).await
    }

    /// Query IndexedDB for events that are related to the given event in the
    /// given room.
    pub async fn get_events_by_related_event(
        &self,
        room_id: &RoomId,
        related_event_id: &EventId,
    ) -> Result<Vec<Event>, TransactionError> {
        let range = IndexedKeyRange::all_with_prefix(
            (room_id, related_event_id),
            self.serializer().inner(),
        );
        self.get_items_by_key::<Event, IndexedEventRelationKey>(range).await
    }

    /// Puts an event in IndexedDB. If an event with the same key already
    /// exists, it will be overwritten. When the item is successfully put, the
    /// function returns the intermediary type [`IndexedEvent`] in case
    /// inspection is needed.
    pub async fn put_event(&self, event: &Event) -> Result<IndexedEvent, TransactionError> {
        if let Some(position) = event.position() {
            // For some reason, we can't simply replace an event with `put_item`
            // because we can get an error stating that the data violates a uniqueness
            // constraint on the `events_position` index. This is NOT expected, but
            // it is not clear if this improperly implemented in the browser or the
            // library we are using.
            //
            // As a workaround, if the event has a position, we delete it first and
            // then call `put_item`. This should be fine as it all happens within the
            // context of a single transaction.
            self.delete_event_by_position(event.linked_chunk_id(), position).await?;
        }
        self.put_item(event).await
    }

    /// Delete events in the given position range matching the given linked
    /// chunk id
    pub async fn delete_events_by_position(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        range: impl Into<IndexedKeyRange<Position>>,
    ) -> Result<(), TransactionError> {
        self.delete_items_by_key_components::<Event, IndexedEventPositionKey>(
            range.into().map(|position| (linked_chunk_id, position)),
        )
        .await
    }

    /// Delete event in the given position matching the given linked chunk id
    pub async fn delete_event_by_position(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        position: Position,
    ) -> Result<(), TransactionError> {
        self.delete_item_by_key::<Event, IndexedEventPositionKey>((linked_chunk_id, position)).await
    }

    /// Delete events in the given chunk matching the given linked chunk id
    pub async fn delete_events_by_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        chunk_id: ChunkIdentifier,
    ) -> Result<(), TransactionError> {
        let range = IndexedKeyRange::all_with_prefix(
            (linked_chunk_id, chunk_id),
            self.serializer().inner(),
        );
        self.delete_items_by_key::<Event, IndexedEventPositionKey>(range).await
    }

    /// Delete events matching the given linked chunk id starting from the given
    /// position until the end of the chunk
    pub async fn delete_events_by_chunk_from_index(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        position: Position,
    ) -> Result<(), TransactionError> {
        let lower = (linked_chunk_id, position);
        let upper = IndexedEventPositionKey::upper_key_components_with_prefix((
            linked_chunk_id,
            ChunkIdentifier::new(position.chunk_identifier),
        ));
        let range = IndexedKeyRange::Bound(lower, upper).map(|(_, position)| position);
        self.delete_events_by_position(linked_chunk_id, range).await
    }

    /// Delete all events matching the given linked chunk id
    pub async fn delete_events_by_linked_chunk_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(), TransactionError> {
        self.delete_items_by_linked_chunk_id::<Event, IndexedEventIdKey>(linked_chunk_id).await
    }

    /// Query IndexedDB for the gap in the given chunk matching the given linked
    /// chunk id.
    pub async fn get_gap_by_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        chunk_id: ChunkIdentifier,
    ) -> Result<Option<Gap>, TransactionError> {
        self.get_item_by_key_components::<Gap, IndexedGapIdKey>((linked_chunk_id, chunk_id)).await
    }

    /// Delete gap that matches the given chunk identifier and the given linked
    /// chunk id
    pub async fn delete_gap_by_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        chunk_id: ChunkIdentifier,
    ) -> Result<(), TransactionError> {
        self.delete_item_by_key::<Gap, IndexedGapIdKey>((linked_chunk_id, chunk_id)).await
    }

    /// Delete all gaps matching the given linked chunk id
    pub async fn delete_gaps_by_linked_chunk_id(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(), TransactionError> {
        self.delete_items_by_linked_chunk_id::<Gap, IndexedGapIdKey>(linked_chunk_id).await
    }
}
