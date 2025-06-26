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
    event_cache::{Event as RawEvent, Gap as RawGap},
    linked_chunk::{ChunkContent, ChunkIdentifier, RawChunk},
};
use ruma::{events::relation::RelationType, OwnedEventId, RoomId};
use serde::{de::DeserializeOwned, Serialize};
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
}

impl From<web_sys::DomException> for IndexeddbEventCacheStoreTransactionError {
    fn from(value: web_sys::DomException) -> Self {
        Self::DomException { name: value.name(), message: value.message(), code: value.code() }
    }
}

impl From<serde_wasm_bindgen::Error> for IndexeddbEventCacheStoreTransactionError {
    fn from(e: serde_wasm_bindgen::Error) -> Self {
        Self::Serialization(Box::new(<serde_json::Error as serde::de::Error>::custom(
            e.to_string(),
        )))
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
}
