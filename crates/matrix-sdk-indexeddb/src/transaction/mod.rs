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

use futures_util::TryStreamExt;
use indexed_db_futures::{
    BuildSerde, cursor::CursorDirection, internals::SystemRepr, query_source::QuerySource,
    transaction as inner,
};
use serde::{
    Serialize,
    de::{DeserializeOwned, Error},
};
use thiserror::Error;
use wasm_bindgen::JsValue;

use crate::{
    error::{AsyncErrorDeps, GenericError},
    serializer::indexed_type::{
        IndexedTypeSerializer,
        range::IndexedKeyRange,
        traits::{Indexed, IndexedKey},
    },
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
    #[error("a numerical operation overflowed")]
    NumericalOverflow,
    #[error("backend: {0}")]
    Backend(Box<dyn AsyncErrorDeps>),
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

impl From<indexed_db_futures::error::SerialisationError> for TransactionError {
    fn from(e: indexed_db_futures::error::SerialisationError) -> Self {
        Self::Serialization(Box::new(serde_json::Error::custom(e.to_string())))
    }
}

impl From<indexed_db_futures::error::JSError> for TransactionError {
    fn from(value: indexed_db_futures::error::JSError) -> Self {
        Self::Backend(Box::new(GenericError::from(value.to_string())))
    }
}

impl From<indexed_db_futures::error::Error> for TransactionError {
    fn from(value: indexed_db_futures::error::Error) -> Self {
        use indexed_db_futures::error::Error;
        match value {
            Error::DomException(e) => e.into_sys().into(),
            Error::Serialisation(e) => e.into(),
            Error::MissingData(e) => Self::Backend(Box::new(e)),
            Error::Unknown(e) => e.into(),
        }
    }
}

/// Represents an IndexedDB transaction, but provides a convenient interface for
/// performing operations on types that implement [`Indexed`] and related
/// traits.
pub struct Transaction<'a> {
    transaction: inner::Transaction<'a>,
    serializer: &'a IndexedTypeSerializer,
}

impl<'a> Transaction<'a> {
    pub fn new(transaction: inner::Transaction<'a>, serializer: &'a IndexedTypeSerializer) -> Self {
        Self { transaction, serializer }
    }

    /// Returns the serializer performing (de)serialization for this
    /// [`Transaction`]
    pub fn serializer(&self) -> &IndexedTypeSerializer {
        self.serializer
    }

    /// Returns the underlying IndexedDB transaction.
    pub fn into_inner(self) -> inner::Transaction<'a> {
        self.transaction
    }

    /// Commit all operations tracked in this transaction to IndexedDB.
    pub async fn commit(self) -> Result<(), TransactionError> {
        self.transaction.commit().await.map_err(Into::into)
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
        let range = self.serializer.encode_key_range::<T, K>(range);
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        let array = if let Some(index) = K::INDEX {
            object_store.index(index)?.get_all().with_query(range).serde()?.await?
        } else {
            object_store.get_all().with_query(range).serde()?.await?
        };
        let mut items = Vec::with_capacity(array.len());
        for value in array {
            let item = T::from_indexed(value?, self.serializer.inner())
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
        let range = self.serializer.encode_key_range::<T, K>(range);
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        let count = if let Some(index) = K::INDEX {
            object_store.index(index)?.count().with_query(range).serde()?.await?
        } else {
            object_store.count().with_query(range).serde()?.await?
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
        K: IndexedKey<T> + Serialize + DeserializeOwned,
    {
        if let Some(key) = self.get_max_key::<T, K>(range).await? {
            return self.get_item_by_key::<T, K>(key).await;
        }
        Ok(None)
    }

    /// Query IndexedDB for keys that match the given key range.
    pub async fn get_keys<T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K>>,
    ) -> Result<Vec<K>, TransactionError>
    where
        T: Indexed,
        K: IndexedKey<T> + Serialize + DeserializeOwned,
    {
        let range = self.serializer.encode_key_range::<T, K>(range);
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        if let Some(index) = K::INDEX {
            let index = object_store.index(index)?;
            if let Some(cursor) = index.open_key_cursor().with_query(range).serde()?.await? {
                return cursor.key_stream_ser().try_collect().await.map_err(Into::into);
            }
        } else if let Some(cursor) =
            object_store.open_key_cursor().with_query(range).serde()?.await?
        {
            return cursor.key_stream_ser().try_collect().await.map_err(Into::into);
        }
        Ok(Vec::new())
    }

    /// Query IndexedDB for the maximum key in the given range.
    pub async fn get_max_key<T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K>>,
    ) -> Result<Option<K>, TransactionError>
    where
        T: Indexed,
        K: IndexedKey<T> + Serialize + DeserializeOwned,
    {
        let range = self.serializer.encode_key_range::<T, K>(range);
        let direction = CursorDirection::Prev;
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        if let Some(index) = K::INDEX {
            let index = object_store.index(index)?;
            if let Some(mut cursor) =
                index.open_key_cursor().with_query(range).with_direction(direction).serde()?.await?
            {
                return cursor.next_key_ser().await.map_err(Into::into);
            }
        } else if let Some(mut cursor) = object_store
            .open_key_cursor()
            .with_query(range)
            .with_direction(direction)
            .serde()?
            .await?
        {
            return cursor.next_key_ser().await.map_err(Into::into);
        }
        Ok(None)
    }

    /// Query IndexedDB for keys that match the given key range. Iterate over
    /// the keys in the given [`direction`](CursorDirection) using a cursor and
    /// fold them into an accumulator while the given function `f` returns
    /// [`Some`].
    ///
    /// This function returns the final value of the accumulator and the key, if
    /// any, which caused the fold to short circuit.
    ///
    /// Note that the use of cursor means that keys are read lazily from
    /// IndexedDB.
    pub async fn fold_keys_while<T, K, Acc, F>(
        &self,
        direction: CursorDirection,
        range: impl Into<IndexedKeyRange<K>>,
        init: Acc,
        mut f: F,
    ) -> Result<(Acc, Option<K>), TransactionError>
    where
        T: Indexed,
        K: IndexedKey<T> + Serialize + DeserializeOwned,
        F: FnMut(&Acc, &K) -> Option<Acc>,
    {
        let range = self.serializer.encode_key_range::<T, K>(range);
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;

        let mut state = init;
        if let Some(index) = K::INDEX {
            let index = object_store.index(index)?;
            if let Some(mut cursor) =
                index.open_key_cursor().with_query(range).with_direction(direction).serde()?.await?
            {
                while let Some(key) = cursor.next_key_ser().await? {
                    match f(&state, &key) {
                        Some(s) => state = s,
                        None => return Ok((state, Some(key))),
                    }
                }
            }
        } else if let Some(mut cursor) = object_store
            .open_key_cursor()
            .with_query(range)
            .with_direction(direction)
            .serde()?
            .await?
        {
            while let Some(key) = cursor.next_key_ser().await? {
                match f(&state, &key) {
                    Some(s) => state = s,
                    None => return Ok((state, Some(key))),
                }
            }
        }
        Ok((state, None))
    }

    /// Adds an item to the corresponding IndexedDB object
    /// store, i.e., `T::OBJECT_STORE`. If an item with the same key already
    /// exists, it will be rejected. When the item is successfully added, the
    /// function returns the intermediary type [`Indexed::IndexedType`] in case
    /// inspection is needed.
    pub async fn add_item<T>(&self, item: &T) -> Result<T::IndexedType, TransactionError>
    where
        T: Indexed + Serialize,
        T::IndexedType: Serialize,
        T::Error: AsyncErrorDeps,
    {
        let output = self
            .serializer
            .serialize(item)
            .map_err(|e| TransactionError::Serialization(Box::new(e)))?;
        self.transaction.object_store(T::OBJECT_STORE)?.add(output.value).await?;
        Ok(output.indexed)
    }

    /// Puts an item in the corresponding IndexedDB object
    /// store, i.e., `T::OBJECT_STORE`. If an item with the same key already
    /// exists, it will be overwritten. When the item is successfully put, the
    /// function returns the intermediary type [`Indexed::IndexedType`] in case
    /// inspection is needed.
    pub async fn put_item<T>(&self, item: &T) -> Result<T::IndexedType, TransactionError>
    where
        T: Indexed + Serialize,
        T::IndexedType: Serialize,
        T::Error: AsyncErrorDeps,
    {
        let output = self
            .serializer
            .serialize(item)
            .map_err(|e| TransactionError::Serialization(Box::new(e)))?;
        self.transaction.object_store(T::OBJECT_STORE)?.put(output.value).await?;
        Ok(output.indexed)
    }

    /// Puts an item in the corresponding IndexedDB object
    /// store, i.e., `T::OBJECT_STORE`, if `T::IndexedType` meets the criteria
    /// defined by `f`. If an item with the same key already
    /// exists, it will be overwritten. When the item is successfully put, the
    /// function returns the intermediary type [`Indexed::IndexedType`] in case
    /// inspection is needed.
    pub async fn put_item_if<T>(
        &self,
        item: &T,
        f: impl Fn(&T::IndexedType) -> bool,
    ) -> Result<Option<T::IndexedType>, TransactionError>
    where
        T: Indexed + Serialize,
        T::IndexedType: Serialize,
        T::Error: AsyncErrorDeps,
    {
        let option = self
            .serializer
            .serialize_if(item, f)
            .map_err(|e| TransactionError::Serialization(Box::new(e)))?;
        if let Some(output) = option {
            self.transaction.object_store(T::OBJECT_STORE)?.put(output.value).await?;
            Ok(Some(output.indexed))
        } else {
            Ok(None)
        }
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
        let range = self.serializer.encode_key_range::<T, K>(range);
        let object_store = self.transaction.object_store(T::OBJECT_STORE)?;
        if let Some(index) = K::INDEX {
            let index = object_store.index(index)?;
            if let Some(mut cursor) = index.open_cursor().with_query(range).serde()?.await? {
                loop {
                    cursor.delete()?;
                    if cursor.next_record::<JsValue>().await?.is_none() {
                        break;
                    }
                }
            }
        } else {
            object_store.delete(range).serde()?.await?;
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
