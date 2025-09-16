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

// Allow dead code here, as this module is still in the process
// of being developed, so some functions will be used later on.
// Once development is complete, we can remove this line and
// clean up any dead code.
#![allow(dead_code)]

use indexed_db_futures::{prelude::IdbTransaction, IdbQuerySource};
use serde::{
    de::{DeserializeOwned, Error},
    Serialize,
};
use thiserror::Error;
use web_sys::IdbCursorDirection;

use crate::{
    error::AsyncErrorDeps,
    serializer::{Indexed, IndexedKey, IndexedKeyRange, IndexedTypeSerializer},
};

#[derive(Debug, Error)]
pub enum TransactionError {
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error("serialization: {0}")]
    Serialization(Box<dyn AsyncErrorDeps>),
    #[error("item is not unique")]
    ItemIsNotUnique,
    #[error("item not found")]
    ItemNotFound,
}

impl From<web_sys::DomException> for TransactionError {
    fn from(value: web_sys::DomException) -> Self {
        Self::DomException { name: value.name(), message: value.message(), code: value.code() }
    }
}

impl From<serde_wasm_bindgen::Error> for TransactionError {
    fn from(e: serde_wasm_bindgen::Error) -> Self {
        Self::Serialization(Box::new(serde_json::Error::custom(e.to_string())))
    }
}

/// Represents an IndexedDB transaction, but provides a convenient interface for
/// performing operations on types that implement [`Indexed`] and related
/// traits.
pub struct Transaction<'a> {
    transaction: IdbTransaction<'a>,
    serializer: &'a IndexedTypeSerializer,
}

impl<'a> Transaction<'a> {
    pub fn new(transaction: IdbTransaction<'a>, serializer: &'a IndexedTypeSerializer) -> Self {
        Self { transaction, serializer }
    }

    /// Returns the serializer performing (de)serialization for this
    /// [`Transaction`]
    pub fn serializer(&self) -> &IndexedTypeSerializer {
        self.serializer
    }

    /// Returns the underlying IndexedDB transaction.
    pub fn into_inner(self) -> IdbTransaction<'a> {
        self.transaction
    }

    /// Commit all operations tracked in this transaction to IndexedDB.
    pub async fn commit(self) -> Result<(), TransactionError> {
        self.transaction.await.into_result().map_err(Into::into)
    }

    /// Query IndexedDB for items that match the given key range
    pub async fn get_items_by_key<T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K>>,
    ) -> Result<Vec<T>, TransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKey<T> + Serialize,
    {
        let range = self.serializer.encode_key_range::<T, K>(range)?;
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        let array = if let Some(index) = K::INDEX {
            object_store.index(index)?.get_all_with_key(&range)?.await?
        } else {
            object_store.get_all_with_key(&range)?.await?
        };
        let mut items = Vec::with_capacity(array.length() as usize);
        for value in array {
            let item = self
                .serializer
                .deserialize(value)
                .map_err(|e| TransactionError::Serialization(Box::new(e)))?;
            items.push(item);
        }
        Ok(items)
    }

    /// Query IndexedDB for items that match the given key component range
    pub async fn get_items_by_key_components<'b, T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K::KeyComponents<'b>>>,
    ) -> Result<Vec<T>, TransactionError>
    where
        T: Indexed + 'b,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKey<T> + Serialize + 'b,
    {
        let range: IndexedKeyRange<K> = range.into().encoded(self.serializer.inner());
        self.get_items_by_key::<T, K>(range).await
    }

    /// Query IndexedDB for items that match the given key. If
    /// more than one item is found, an error is returned.
    pub async fn get_item_by_key<T, K>(&self, key: K) -> Result<Option<T>, TransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKey<T> + Serialize,
    {
        let mut items = self.get_items_by_key::<T, K>(key).await?;
        if items.len() > 1 {
            return Err(TransactionError::ItemIsNotUnique);
        }
        Ok(items.pop())
    }

    /// Query IndexedDB for items that match the given key components. If more
    /// than one item is found, an error is returned.
    pub async fn get_item_by_key_components<'b, T, K>(
        &self,
        components: K::KeyComponents<'b>,
    ) -> Result<Option<T>, TransactionError>
    where
        T: Indexed + 'b,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKey<T> + Serialize + 'b,
    {
        let mut items = self.get_items_by_key_components::<T, K>(components).await?;
        if items.len() > 1 {
            return Err(TransactionError::ItemIsNotUnique);
        }
        Ok(items.pop())
    }

    /// Query IndexedDB for the number of items that match the given key range.
    pub async fn get_items_count_by_key<T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K>>,
    ) -> Result<usize, TransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKey<T> + Serialize,
    {
        let range = self.serializer.encode_key_range::<T, K>(range)?;
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        let count = if let Some(index) = K::INDEX {
            object_store.index(index)?.count_with_key(&range)?.await?
        } else {
            object_store.count_with_key(&range)?.await?
        };
        Ok(count as usize)
    }

    /// Query IndexedDB for the number of items that match the given key
    /// components range.
    pub async fn get_items_count_by_key_components<'b, T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K::KeyComponents<'b>>>,
    ) -> Result<usize, TransactionError>
    where
        T: Indexed + 'b,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKey<T> + Serialize + 'b,
    {
        let range: IndexedKeyRange<K> = range.into().encoded(self.serializer.inner());
        self.get_items_count_by_key::<T, K>(range).await
    }

    /// Query IndexedDB for the item with the maximum key in the given range.
    pub async fn get_max_item_by_key<T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K>>,
    ) -> Result<Option<T>, TransactionError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: AsyncErrorDeps,
        K: IndexedKey<T> + Serialize,
    {
        let range = self.serializer.encode_key_range::<T, K>(range)?;
        let direction = IdbCursorDirection::Prev;
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        if let Some(index) = K::INDEX {
            object_store
                .index(index)?
                .open_cursor_with_range_and_direction(&range, direction)?
                .await?
                .map(|cursor| self.serializer.deserialize(cursor.value()))
                .transpose()
                .map_err(|e| TransactionError::Serialization(Box::new(e)))
        } else {
            object_store
                .open_cursor_with_range_and_direction(&range, direction)?
                .await?
                .map(|cursor| self.serializer.deserialize(cursor.value()))
                .transpose()
                .map_err(|e| TransactionError::Serialization(Box::new(e)))
        }
    }

    /// Adds an item to the corresponding IndexedDB object
    /// store, i.e., `T::OBJECT_STORE`. If an item with the same key already
    /// exists, it will be rejected.
    pub async fn add_item<T>(&self, item: &T) -> Result<(), TransactionError>
    where
        T: Indexed + Serialize,
        T::IndexedType: Serialize,
        T::Error: AsyncErrorDeps,
    {
        self.transaction
            .object_store(T::OBJECT_STORE)?
            .add_val_owned(
                self.serializer
                    .serialize(item)
                    .map_err(|e| TransactionError::Serialization(Box::new(e)))?,
            )?
            .await
            .map_err(Into::into)
    }

    /// Puts an item in the corresponding IndexedDB object
    /// store, i.e., `T::OBJECT_STORE`. If an item with the same key already
    /// exists, it will be overwritten.
    pub async fn put_item<T>(&self, item: &T) -> Result<(), TransactionError>
    where
        T: Indexed + Serialize,
        T::IndexedType: Serialize,
        T::Error: AsyncErrorDeps,
    {
        self.transaction
            .object_store(T::OBJECT_STORE)?
            .put_val_owned(
                self.serializer
                    .serialize(item)
                    .map_err(|e| TransactionError::Serialization(Box::new(e)))?,
            )?
            .await
            .map_err(Into::into)
    }

    /// Delete items in given key range from IndexedDB
    pub async fn delete_items_by_key<T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K>>,
    ) -> Result<(), TransactionError>
    where
        T: Indexed,
        K: IndexedKey<T> + Serialize,
    {
        let range = self.serializer.encode_key_range::<T, K>(range)?;
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

    /// Delete items in the given key component range from
    /// IndexedDB
    pub async fn delete_items_by_key_components<'b, T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K::KeyComponents<'b>>>,
    ) -> Result<(), TransactionError>
    where
        T: Indexed + 'b,
        K: IndexedKey<T> + Serialize + 'b,
    {
        let range: IndexedKeyRange<K> = range.into().encoded(self.serializer.inner());
        self.delete_items_by_key::<T, K>(range).await
    }

    /// Delete item that matches the given key components from
    /// IndexedDB
    pub async fn delete_item_by_key<'b, T, K>(
        &self,
        key: K::KeyComponents<'b>,
    ) -> Result<(), TransactionError>
    where
        T: Indexed + 'b,
        K: IndexedKey<T> + Serialize + 'b,
    {
        self.delete_items_by_key_components::<T, K>(key).await
    }

    /// Clear all items of type `T` from the associated object store
    /// `T::OBJECT_STORE` from IndexedDB
    pub async fn clear<T>(&self) -> Result<(), TransactionError>
    where
        T: Indexed,
    {
        self.transaction.object_store(T::OBJECT_STORE)?.clear()?.await.map_err(Into::into)
    }
}
