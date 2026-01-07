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

pub mod constants;
pub mod range;
pub mod traits;

use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::KeyRange;
use range::IndexedKeyRange;
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use traits::{Indexed, IndexedKey};
use wasm_bindgen::JsValue;

use crate::serializer::safe_encode::types::SafeEncodeSerializer;

#[derive(Debug, Error)]
pub enum IndexedTypeSerializerError<IndexingError> {
    #[error("indexing: {0}")]
    Indexing(IndexingError),
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl<T> From<serde_wasm_bindgen::Error> for IndexedTypeSerializerError<T> {
    fn from(e: serde_wasm_bindgen::Error) -> Self {
        Self::Serialization(serde::de::Error::custom(e.to_string()))
    }
}

/// A type encapsulating the output of serializing a value through
/// [`IndexedTypeSerializer`]. This contains both the intermediary type - i.e.,
/// [`Indexed::IndexedType`] - and the fully-serialized type - i.e.,
/// [`JsValue`]. It is convenient for cases where one may want to examine the
/// intermediary type.
#[derive(Debug)]
pub struct IndexedTypeSerializationOutput<T: Indexed> {
    /// The intermediary value created in the process of serialization
    pub indexed: T::IndexedType,
    /// The fully-serialized value
    pub value: JsValue,
}

/// A (de)serializer for an IndexedDB implementation of [`EventCacheStore`][1].
///
/// This is primarily a wrapper around [`SafeEncodeSerializer`] with
/// convenience functions for (de)serializing types specific to the
/// [`EventCacheStore`][1].
///
/// [1]: matrix_sdk_base::event_cache::store::EventCacheStore
#[derive(Debug, Clone)]
pub struct IndexedTypeSerializer {
    inner: SafeEncodeSerializer,
}

impl IndexedTypeSerializer {
    pub fn new(inner: SafeEncodeSerializer) -> Self {
        Self { inner }
    }

    /// Returns a reference to the inner [`IndexeddbSerializer`].
    pub fn inner(&self) -> &SafeEncodeSerializer {
        &self.inner
    }

    /// Encodes an key for a [`Indexed`] type.
    ///
    /// Note that the particular key which is encoded is defined by the type
    /// `K`.
    pub fn encode_key<T, K>(&self, components: K::KeyComponents<'_>) -> K
    where
        T: Indexed,
        K: IndexedKey<T>,
    {
        K::encode(components, &self.inner)
    }

    /// Encodes a key for a [`Indexed`] type as a [`JsValue`].
    ///
    /// Note that the particular key which is encoded is defined by the type
    /// `K`.
    pub fn encode_key_as_value<T, K>(
        &self,
        components: K::KeyComponents<'_>,
    ) -> Result<JsValue, serde_wasm_bindgen::Error>
    where
        T: Indexed,
        K: IndexedKey<T> + Serialize,
    {
        serde_wasm_bindgen::to_value(&self.encode_key::<T, K>(components))
    }

    /// Encodes a key component range for an [`Indexed`] type.
    ///
    /// Note that the particular key which is encoded is defined by the type
    /// `K`.
    pub fn encode_key_range<T, K>(&self, range: impl Into<IndexedKeyRange<K>>) -> KeyRange<K>
    where
        T: Indexed,
        K: Serialize,
    {
        match range.into() {
            IndexedKeyRange::Only(key) => KeyRange::Only(key),
            IndexedKeyRange::Bound(lower, upper) => KeyRange::Bound(lower, false, upper, false),
        }
    }

    /// Encodes a key component range for an [`Indexed`] type.
    ///
    /// Note that the particular key which is encoded is defined by the type
    /// `K`.
    pub fn encode_key_component_range<'a, T, K>(
        &self,
        range: impl Into<IndexedKeyRange<K::KeyComponents<'a>>>,
    ) -> KeyRange<K>
    where
        T: Indexed,
        K: IndexedKey<T> + Serialize,
    {
        let range = match range.into() {
            IndexedKeyRange::Only(components) => {
                IndexedKeyRange::Only(K::encode(components, &self.inner))
            }
            IndexedKeyRange::Bound(lower, upper) => {
                let lower = K::encode(lower, &self.inner);
                let upper = K::encode(upper, &self.inner);
                IndexedKeyRange::Bound(lower, upper)
            }
        };
        self.encode_key_range::<T, K>(range)
    }

    /// Serializes an [`Indexed`] type into a [`JsValue`] and returns both the
    /// [`JsValue`] and the intermediary [`Indexed::IndexedType`]
    pub fn serialize<T>(
        &self,
        t: &T,
    ) -> Result<IndexedTypeSerializationOutput<T>, IndexedTypeSerializerError<T::Error>>
    where
        T: Indexed,
        T::IndexedType: Serialize,
    {
        let indexed = t.to_indexed(&self.inner).map_err(IndexedTypeSerializerError::Indexing)?;
        let value = serde_wasm_bindgen::to_value(&indexed)?;
        Ok(IndexedTypeSerializationOutput { indexed, value })
    }

    /// Serializes an [`Indexed`] type into a [`JsValue`] if the
    /// [`Indexed::IndexedType`] meets criteria defined by `f`. If successful,
    /// returns both the [`JsValue`] and the [`Indexed::IndexedType`].
    pub fn serialize_if<T>(
        &self,
        t: &T,
        f: impl Fn(&T::IndexedType) -> bool,
    ) -> Result<Option<IndexedTypeSerializationOutput<T>>, IndexedTypeSerializerError<T::Error>>
    where
        T: Indexed,
        T::IndexedType: Serialize,
    {
        let indexed = t.to_indexed(&self.inner).map_err(IndexedTypeSerializerError::Indexing)?;
        if f(&indexed) {
            let value = serde_wasm_bindgen::to_value(&indexed)?;
            Ok(Some(IndexedTypeSerializationOutput { indexed, value }))
        } else {
            Ok(None)
        }
    }

    /// Deserializes an [`Indexed`] type from a [`JsValue`]
    pub fn deserialize<T>(&self, value: JsValue) -> Result<T, IndexedTypeSerializerError<T::Error>>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
    {
        let indexed: T::IndexedType = value.into_serde()?;
        T::from_indexed(indexed, &self.inner).map_err(IndexedTypeSerializerError::Indexing)
    }
}
