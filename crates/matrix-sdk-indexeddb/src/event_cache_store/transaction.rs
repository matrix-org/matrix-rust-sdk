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

use indexed_db_futures::{prelude::IdbTransaction, IdbQuerySource};
use matrix_sdk_base::{
    event_cache::{store::EventCacheStoreError, Event as RawEvent, Gap as RawGap},
    linked_chunk::{ChunkContent, ChunkIdentifier, RawChunk},
};
use ruma::{events::relation::RelationType, OwnedEventId, RoomId};
use serde::{
    de::{DeserializeOwned, Error},
    Serialize,
};
use thiserror::Error;
use web_sys::IdbCursorDirection;

use crate::event_cache_store::{
    error::AsyncErrorDeps,
    serializer::{
        traits::{Indexed, IndexedKey, IndexedKeyBounds, IndexedKeyComponentBounds},
        types::{
            IndexedChunkIdKey, IndexedEventIdKey, IndexedEventPositionKey, IndexedEventRelationKey,
            IndexedGapIdKey, IndexedKeyRange, IndexedNextChunkIdKey,
        },
        IndexeddbEventCacheStoreSerializer,
    },
    types::{Chunk, ChunkType, Event, Gap, Position},
};

#[derive(Debug, Error)]
pub enum IndexeddbEventCacheStoreTransactionError {
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error("serialization: {0}")]
    Serialization(Box<dyn AsyncErrorDeps>),
    #[error("item is not unique")]
    ItemIsNotUnique,
    #[error("item not found")]
    ItemNotFound,
}

impl From<web_sys::DomException> for IndexeddbEventCacheStoreTransactionError {
    fn from(value: web_sys::DomException) -> Self {
        Self::DomException { name: value.name(), message: value.message(), code: value.code() }
    }
}

impl From<serde_wasm_bindgen::Error> for IndexeddbEventCacheStoreTransactionError {
    fn from(e: serde_wasm_bindgen::Error) -> Self {
        Self::Serialization(Box::new(serde_json::Error::custom(e.to_string())))
    }
}

impl From<IndexeddbEventCacheStoreTransactionError> for EventCacheStoreError {
    fn from(value: IndexeddbEventCacheStoreTransactionError) -> Self {
        use IndexeddbEventCacheStoreTransactionError::*;

        match value {
            DomException { .. } => Self::InvalidData { details: value.to_string() },
            Serialization(e) => Self::Serialization(serde_json::Error::custom(e.to_string())),
            ItemIsNotUnique | ItemNotFound => Self::InvalidData { details: value.to_string() },
        }
    }
}

/// Represents an IndexedDB transaction, but provides a convenient interface for
/// performing operations relevant to the IndexedDB implementation of
/// [`EventCacheStore`](matrix_sdk_base::event_cache::store::EventCacheStore).
pub struct IndexeddbEventCacheStoreTransaction<'a> {
    transaction: IdbTransaction<'a>,
    serializer: &'a IndexeddbEventCacheStoreSerializer,
}

impl<'a> IndexeddbEventCacheStoreTransaction<'a> {
    pub fn new(
        transaction: IdbTransaction<'a>,
        serializer: &'a IndexeddbEventCacheStoreSerializer,
    ) -> Self {
        Self { transaction, serializer }
    }

    /// Returns the underlying IndexedDB transaction.
    pub fn into_inner(self) -> IdbTransaction<'a> {
        self.transaction
    }

    /// Commit all operations tracked in this transaction to IndexedDB.
    pub async fn commit(self) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        self.transaction.await.into_result().map_err(Into::into)
    }

    /// Query IndexedDB for items that match the given key range in the given
    /// room.
    pub async fn get_items_by_key<T, K>(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<K>>,
    ) -> Result<Vec<T>, IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKeyBounds<T> + Serialize,
    {
        let range = self.serializer.encode_key_range::<T, K>(room_id, range)?;
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        let array = if let Some(index) = K::INDEX {
            object_store.index(index)?.get_all_with_key(&range)?.await?
        } else {
            object_store.get_all_with_key(&range)?.await?
        };
        let mut items = Vec::with_capacity(array.length() as usize);
        for value in array {
            let item = self.serializer.deserialize(value).map_err(|e| {
                IndexeddbEventCacheStoreTransactionError::Serialization(Box::new(e))
            })?;
            items.push(item);
        }
        Ok(items)
    }

    /// Query IndexedDB for items that match the given key component range in
    /// the given room.
    pub async fn get_items_by_key_components<'b, T, K>(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<&'b K::KeyComponents>>,
    ) -> Result<Vec<T>, IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKeyComponentBounds<T> + Serialize,
        K::KeyComponents: 'b,
    {
        let range: IndexedKeyRange<K> = range.into().encoded(room_id, self.serializer.inner());
        self.get_items_by_key::<T, K>(room_id, range).await
    }

    /// Query IndexedDB for all items in the given room by key `K`
    pub async fn get_items_in_room<T, K>(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<T>, IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKeyBounds<T> + Serialize,
    {
        self.get_items_by_key::<T, K>(room_id, IndexedKeyRange::All).await
    }

    /// Query IndexedDB for items that match the given key in the given room. If
    /// more than one item is found, an error is returned.
    pub async fn get_item_by_key<T, K>(
        &self,
        room_id: &RoomId,
        key: K,
    ) -> Result<Option<T>, IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKeyBounds<T> + Serialize,
    {
        let mut items = self.get_items_by_key::<T, K>(room_id, key).await?;
        if items.len() > 1 {
            return Err(IndexeddbEventCacheStoreTransactionError::ItemIsNotUnique);
        }
        Ok(items.pop())
    }

    /// Query IndexedDB for items that match the given key components in the
    /// given room. If more than one item is found, an error is returned.
    pub async fn get_item_by_key_components<T, K>(
        &self,
        room_id: &RoomId,
        components: &K::KeyComponents,
    ) -> Result<Option<T>, IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKeyComponentBounds<T> + Serialize,
    {
        let mut items = self.get_items_by_key_components::<T, K>(room_id, components).await?;
        if items.len() > 1 {
            return Err(IndexeddbEventCacheStoreTransactionError::ItemIsNotUnique);
        }
        Ok(items.pop())
    }

    /// Query IndexedDB for the number of items that match the given key range
    /// in the given room.
    pub async fn get_items_count_by_key<T, K>(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<K>>,
    ) -> Result<usize, IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKeyBounds<T> + Serialize,
    {
        let range = self.serializer.encode_key_range::<T, K>(room_id, range)?;
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        let count = if let Some(index) = K::INDEX {
            object_store.index(index)?.count_with_key(&range)?.await?
        } else {
            object_store.count_with_key(&range)?.await?
        };
        Ok(count as usize)
    }

    /// Query IndexedDB for the number of items that match the given key
    /// components range in the given room.
    pub async fn get_items_count_by_key_components<'b, T, K>(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<&'b K::KeyComponents>>,
    ) -> Result<usize, IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKeyBounds<T> + Serialize,
        K::KeyComponents: 'b,
    {
        let range: IndexedKeyRange<K> = range.into().encoded(room_id, self.serializer.inner());
        self.get_items_count_by_key::<T, K>(room_id, range).await
    }

    /// Query IndexedDB for the number of items in the given room.
    pub async fn get_items_count_in_room<T, K>(
        &self,
        room_id: &RoomId,
    ) -> Result<usize, IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKeyBounds<T> + Serialize,
    {
        self.get_items_count_by_key::<T, K>(room_id, IndexedKeyRange::All).await
    }

    /// Query IndexedDB for the item with the maximum key in the given room.
    pub async fn get_max_item_by_key<T, K>(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<T>, IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKey<T> + IndexedKeyBounds<T> + Serialize,
    {
        let range = self.serializer.encode_key_range::<T, K>(room_id, IndexedKeyRange::All)?;
        let direction = IdbCursorDirection::Prev;
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        if let Some(index) = K::INDEX {
            object_store
                .index(index)?
                .open_cursor_with_range_and_direction(&range, direction)?
                .await?
                .map(|cursor| self.serializer.deserialize(cursor.value()))
                .transpose()
                .map_err(|e| IndexeddbEventCacheStoreTransactionError::Serialization(Box::new(e)))
        } else {
            object_store
                .open_cursor_with_range_and_direction(&range, direction)?
                .await?
                .map(|cursor| self.serializer.deserialize(cursor.value()))
                .transpose()
                .map_err(|e| IndexeddbEventCacheStoreTransactionError::Serialization(Box::new(e)))
        }
    }

    /// Adds an item to the given room in the corresponding IndexedDB object
    /// store, i.e., `T::OBJECT_STORE`. If an item with the same key already
    /// exists, it will be rejected.
    pub async fn add_item<T>(
        &self,
        room_id: &RoomId,
        item: &T,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed + Serialize,
        T::IndexedType: Serialize,
        T::Error: AsyncErrorDeps,
    {
        self.transaction
            .object_store(T::OBJECT_STORE)?
            .add_val_owned(self.serializer.serialize(room_id, item).map_err(|e| {
                IndexeddbEventCacheStoreTransactionError::Serialization(Box::new(e))
            })?)?
            .await
            .map_err(Into::into)
    }

    /// Puts an item in the given room in the corresponding IndexedDB object
    /// store, i.e., `T::OBJECT_STORE`. If an item with the same key already
    /// exists, it will be overwritten.
    pub async fn put_item<T>(
        &self,
        room_id: &RoomId,
        item: &T,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed + Serialize,
        T::IndexedType: Serialize,
        T::Error: AsyncErrorDeps,
    {
        self.transaction
            .object_store(T::OBJECT_STORE)?
            .put_val_owned(self.serializer.serialize(room_id, item).map_err(|e| {
                IndexeddbEventCacheStoreTransactionError::Serialization(Box::new(e))
            })?)?
            .await
            .map_err(Into::into)
    }

    /// Delete items in given key range in the given room from IndexedDB
    pub async fn delete_items_by_key<T, K>(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<K>>,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        K: IndexedKeyBounds<T> + Serialize,
    {
        let range = self.serializer.encode_key_range::<T, K>(room_id, range)?;
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        if let Some(index) = K::INDEX {
            let index = object_store.index(index)?;
            if let Some(cursor) = index.open_cursor_with_range(&range)?.await? {
                while cursor.key().is_some() {
                    cursor.delete()?.await?;
                    cursor.continue_cursor()?.await?;
                }
            }
        } else {
            object_store.delete_owned(&range)?.await?;
        }
        Ok(())
    }

    /// Delete items in the given key component range in the given room from
    /// IndexedDB
    pub async fn delete_items_by_key_components<'b, T, K>(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<&'b K::KeyComponents>>,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        K: IndexedKeyBounds<T> + Serialize,
        K::KeyComponents: 'b,
    {
        let range: IndexedKeyRange<K> = range.into().encoded(room_id, self.serializer.inner());
        self.delete_items_by_key::<T, K>(room_id, range).await
    }

    /// Delete all items of type `T` by key `K` in the given room from IndexedDB
    pub async fn delete_items_in_room<T, K>(
        &self,
        room_id: &RoomId,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        K: IndexedKeyBounds<T> + Serialize,
    {
        self.delete_items_by_key::<T, K>(room_id, IndexedKeyRange::All).await
    }

    /// Delete item that matches the given key components in the given room from
    /// IndexedDB
    pub async fn delete_item_by_key<T, K>(
        &self,
        room_id: &RoomId,
        key: &K::KeyComponents,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
        K: IndexedKeyBounds<T> + Serialize,
    {
        self.delete_items_by_key_components::<T, K>(room_id, key).await
    }

    /// Clear all items of type `T` in all rooms from IndexedDB
    pub async fn clear<T>(&self) -> Result<(), IndexeddbEventCacheStoreTransactionError>
    where
        T: Indexed,
    {
        self.transaction.object_store(T::OBJECT_STORE)?.clear()?.await.map_err(Into::into)
    }

    /// Query IndexedDB for chunks that match the given chunk identifier in the
    /// given room. If more than one item is found, an error is returned.
    pub async fn get_chunk_by_id(
        &self,
        room_id: &RoomId,
        chunk_id: &ChunkIdentifier,
    ) -> Result<Option<Chunk>, IndexeddbEventCacheStoreTransactionError> {
        self.get_item_by_key_components::<Chunk, IndexedChunkIdKey>(room_id, chunk_id).await
    }

    /// Query IndexedDB for chunks such that the next chunk matches the given
    /// chunk identifier in the given room. If more than one item is found,
    /// an error is returned.
    pub async fn get_chunk_by_next_chunk_id(
        &self,
        room_id: &RoomId,
        next_chunk_id: &Option<ChunkIdentifier>,
    ) -> Result<Option<Chunk>, IndexeddbEventCacheStoreTransactionError> {
        self.get_item_by_key_components::<Chunk, IndexedNextChunkIdKey>(room_id, next_chunk_id)
            .await
    }

    /// Query IndexedDB for all chunks in the given room
    pub async fn get_chunks_in_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<Chunk>, IndexeddbEventCacheStoreTransactionError> {
        self.get_items_in_room::<Chunk, IndexedChunkIdKey>(room_id).await
    }

    /// Query IndexedDB for the number of chunks in the given room.
    pub async fn get_chunks_count_in_room(
        &self,
        room_id: &RoomId,
    ) -> Result<usize, IndexeddbEventCacheStoreTransactionError> {
        self.get_items_count_in_room::<Chunk, IndexedChunkIdKey>(room_id).await
    }

    /// Query IndexedDB for the chunk with the maximum key in the given room.
    pub async fn get_max_chunk_by_id(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<Chunk>, IndexeddbEventCacheStoreTransactionError> {
        self.get_max_item_by_key::<Chunk, IndexedChunkIdKey>(room_id).await
    }

    /// Query IndexedDB for given chunk in given room and additionally query
    /// for events or gap, depending on chunk type, in order to construct the
    /// full chunk.
    pub async fn load_chunk_by_id(
        &self,
        room_id: &RoomId,
        chunk_id: &ChunkIdentifier,
    ) -> Result<Option<RawChunk<RawEvent, RawGap>>, IndexeddbEventCacheStoreTransactionError> {
        if let Some(chunk) = self.get_chunk_by_id(room_id, chunk_id).await? {
            let content = match chunk.chunk_type {
                ChunkType::Event => {
                    let events = self
                        .get_events_by_chunk(room_id, &ChunkIdentifier::new(chunk.identifier))
                        .await?
                        .into_iter()
                        .map(RawEvent::from)
                        .collect();
                    ChunkContent::Items(events)
                }
                ChunkType::Gap => {
                    let gap = self
                        .get_gap_by_id(room_id, &ChunkIdentifier::new(chunk.identifier))
                        .await?
                        .ok_or(IndexeddbEventCacheStoreTransactionError::ItemNotFound)?;
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

    /// Add a chunk to the given room and ensure that the next and previous
    /// chunks are properly linked to the chunk being added. If a chunk with
    /// the same identifier already exists, the given chunk will be
    /// rejected.
    pub async fn add_chunk(
        &self,
        room_id: &RoomId,
        chunk: &Chunk,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        self.add_item(room_id, chunk).await?;
        if let Some(previous) = chunk.previous {
            let previous_identifier = ChunkIdentifier::new(previous);
            if let Some(mut previous_chunk) =
                self.get_chunk_by_id(room_id, &previous_identifier).await?
            {
                previous_chunk.next = Some(chunk.identifier);
                self.put_item(room_id, &previous_chunk).await?;
            }
        }
        if let Some(next) = chunk.next {
            let next_identifier = ChunkIdentifier::new(next);
            if let Some(mut next_chunk) = self.get_chunk_by_id(room_id, &next_identifier).await? {
                next_chunk.previous = Some(chunk.identifier);
                self.put_item(room_id, &next_chunk).await?;
            }
        }
        Ok(())
    }

    /// Delete chunk that matches the given id in the given room and ensure that
    /// the next and previous chunk are updated to link to one another.
    /// Additionally, ensure that events and gaps in the given chunk are
    /// also deleted.
    pub async fn delete_chunk_by_id(
        &self,
        room_id: &RoomId,
        chunk_id: &ChunkIdentifier,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        if let Some(chunk) = self.get_chunk_by_id(room_id, chunk_id).await? {
            if let Some(previous) = chunk.previous {
                let previous_identifier = ChunkIdentifier::new(previous);
                if let Some(mut previous_chunk) =
                    self.get_chunk_by_id(room_id, &previous_identifier).await?
                {
                    previous_chunk.next = chunk.next;
                    self.put_item(room_id, &previous_chunk).await?;
                }
            }
            if let Some(next) = chunk.next {
                let next_identifier = ChunkIdentifier::new(next);
                if let Some(mut next_chunk) =
                    self.get_chunk_by_id(room_id, &next_identifier).await?
                {
                    next_chunk.previous = chunk.previous;
                    self.put_item(room_id, &next_chunk).await?;
                }
            }
            self.delete_item_by_key::<Chunk, IndexedChunkIdKey>(room_id, chunk_id).await?;
            match chunk.chunk_type {
                ChunkType::Event => {
                    self.delete_events_by_chunk(room_id, chunk_id).await?;
                }
                ChunkType::Gap => {
                    self.delete_gap_by_id(room_id, chunk_id).await?;
                }
            }
        }
        Ok(())
    }

    /// Delete all chunks in the given room
    pub async fn delete_chunks_in_room(
        &self,
        room_id: &RoomId,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        self.delete_items_in_room::<Chunk, IndexedChunkIdKey>(room_id).await
    }

    /// Query IndexedDB for events in the given position range in the given
    /// room.
    pub async fn get_events_by_position(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<&Position>>,
    ) -> Result<Vec<Event>, IndexeddbEventCacheStoreTransactionError> {
        self.get_items_by_key_components::<Event, IndexedEventPositionKey>(room_id, range).await
    }

    /// Query IndexedDB for number of events in the given position range in the
    /// given room.
    pub async fn get_events_count_by_position(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<&Position>>,
    ) -> Result<usize, IndexeddbEventCacheStoreTransactionError> {
        self.get_items_count_by_key_components::<Event, IndexedEventPositionKey>(room_id, range)
            .await
    }

    /// Query IndexedDB for events in the given chunk in the given room.
    pub async fn get_events_by_chunk(
        &self,
        room_id: &RoomId,
        chunk_id: &ChunkIdentifier,
    ) -> Result<Vec<Event>, IndexeddbEventCacheStoreTransactionError> {
        let mut lower = IndexedEventPositionKey::lower_key_components();
        lower.chunk_identifier = chunk_id.index();
        let mut upper = IndexedEventPositionKey::upper_key_components();
        upper.chunk_identifier = chunk_id.index();
        let range = IndexedKeyRange::Bound(&lower, &upper);
        self.get_events_by_position(room_id, range).await
    }

    /// Query IndexedDB for number of events in the given chunk in the given
    /// room.
    pub async fn get_events_count_by_chunk(
        &self,
        room_id: &RoomId,
        chunk_id: &ChunkIdentifier,
    ) -> Result<usize, IndexeddbEventCacheStoreTransactionError> {
        let mut lower = IndexedEventPositionKey::lower_key_components();
        lower.chunk_identifier = chunk_id.index();
        let mut upper = IndexedEventPositionKey::upper_key_components();
        upper.chunk_identifier = chunk_id.index();
        let range = IndexedKeyRange::Bound(&lower, &upper);
        self.get_events_count_by_position(room_id, range).await
    }

    /// Puts an event in the given room. If an event with the same key already
    /// exists, it will be overwritten.
    pub async fn put_event(
        &self,
        room_id: &RoomId,
        event: &Event,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
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
            self.delete_event_by_position(room_id, &position).await?;
        }
        self.put_item(room_id, event).await
    }

    /// Delete events in the given position range in the given room
    pub async fn delete_events_by_position(
        &self,
        room_id: &RoomId,
        range: impl Into<IndexedKeyRange<&Position>>,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        self.delete_items_by_key_components::<Event, IndexedEventPositionKey>(room_id, range).await
    }

    /// Delete event in the given position in the given room
    pub async fn delete_event_by_position(
        &self,
        room_id: &RoomId,
        position: &Position,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        self.delete_item_by_key::<Event, IndexedEventPositionKey>(room_id, position).await
    }

    /// Delete events in the given chunk in the given room
    pub async fn delete_events_by_chunk(
        &self,
        room_id: &RoomId,
        chunk_id: &ChunkIdentifier,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        let mut lower = IndexedEventPositionKey::lower_key_components();
        lower.chunk_identifier = chunk_id.index();
        let mut upper = IndexedEventPositionKey::upper_key_components();
        upper.chunk_identifier = chunk_id.index();
        let range = IndexedKeyRange::Bound(&lower, &upper);
        self.delete_events_by_position(room_id, range).await
    }

    /// Delete events starting from the given position in the given room
    /// until the end of the chunk
    pub async fn delete_events_by_chunk_from_index(
        &self,
        room_id: &RoomId,
        position: &Position,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        let mut upper = IndexedEventPositionKey::upper_key_components();
        upper.chunk_identifier = position.chunk_identifier;
        let range = IndexedKeyRange::Bound(position, &upper);
        self.delete_events_by_position(room_id, range).await
    }

    /// Delete all events in the given room
    pub async fn delete_events_in_room(
        &self,
        room_id: &RoomId,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        self.delete_items_in_room::<Event, IndexedEventIdKey>(room_id).await
    }

    /// Query IndexedDB for the gap in the given chunk in the given room.
    pub async fn get_gap_by_id(
        &self,
        room_id: &RoomId,
        chunk_id: &ChunkIdentifier,
    ) -> Result<Option<Gap>, IndexeddbEventCacheStoreTransactionError> {
        self.get_item_by_key_components::<Gap, IndexedGapIdKey>(room_id, chunk_id).await
    }

    /// Delete gap that matches the given chunk identifier in the given room
    pub async fn delete_gap_by_id(
        &self,
        room_id: &RoomId,
        chunk_id: &ChunkIdentifier,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        self.delete_item_by_key::<Gap, IndexedGapIdKey>(room_id, chunk_id).await
    }

    /// Delete all gaps in the given room
    pub async fn delete_gaps_in_room(
        &self,
        room_id: &RoomId,
    ) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        self.delete_items_in_room::<Gap, IndexedGapIdKey>(room_id).await
    }
}
