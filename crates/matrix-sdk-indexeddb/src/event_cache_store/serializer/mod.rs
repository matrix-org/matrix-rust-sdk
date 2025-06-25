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

use gloo_utils::format::JsValueSerdeExt;
use matrix_sdk_crypto::CryptoStoreError;
use ruma::RoomId;
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;

use crate::{
    event_cache_store::serializer::traits::{Indexed, IndexedKey, IndexedKeyBounds},
    serializer::IndexeddbSerializer,
};

mod traits;
mod types;

#[derive(Debug, Error)]
pub enum IndexeddbEventCacheStoreSerializerError {
    #[error("indexing: {0}")]
    Indexing(Box<dyn std::error::Error>),
    #[error("serialization: {0}")]
    Serialization(#[from] serde_json::Error),
}

impl From<serde_wasm_bindgen::Error> for IndexeddbEventCacheStoreSerializerError {
    fn from(e: serde_wasm_bindgen::Error) -> Self {
        Self::Serialization(serde::de::Error::custom(e.to_string()))
    }
}

/// A (de)serializer for an IndexedDB implementation of [`EventCacheStore`][1].
///
/// This is primarily a wrapper around [`IndexeddbSerializer`] with a
/// convenience functions for (de)serializing types specific to the
/// [`EventCacheStore`][1].
///
/// [1]: matrix_sdk_base::event_cache::store::EventCacheStore
pub struct IndexeddbEventCacheStoreSerializer {
    inner: IndexeddbSerializer,
}

impl IndexeddbEventCacheStoreSerializer {
    pub fn new(inner: IndexeddbSerializer) -> Self {
        Self { inner }
    }

    /// Encodes an key for a [`Indexed`] type.
    ///
    /// Note that the particular key which is encoded is defined by the type
    /// `K`.
    pub fn encode_key<T, K>(&self, room_id: &RoomId, components: &K::KeyComponents) -> K
    where
        T: Indexed,
        K: IndexedKey<T>,
    {
        K::encode(room_id, components, &self.inner)
    }

    /// Encodes a key for a [`Indexed`] type as a [`JsValue`].
    ///
    /// Note that the particular key which is encoded is defined by the type
    /// `K`.
    pub fn encode_key_as_value<T, K>(
        &self,
        room_id: &RoomId,
        components: &K::KeyComponents,
    ) -> Result<JsValue, serde_wasm_bindgen::Error>
    where
        T: Indexed,
        K: IndexedKey<T> + Serialize,
    {
        serde_wasm_bindgen::to_value(&self.encode_key::<T, K>(room_id, components))
    }

    /// Encodes the entire key range for an [`Indexed`] type.
    ///
    /// Note that the particular key which is encoded is defined by the type
    /// `K`.
    pub fn encode_key_range<T, K>(
        &self,
        room_id: &RoomId,
    ) -> Result<IdbKeyRange, serde_wasm_bindgen::Error>
    where
        T: Indexed,
        K: IndexedKeyBounds<T> + Serialize,
    {
        let lower = serde_wasm_bindgen::to_value(&K::encode_lower(room_id, &self.inner))?;
        let upper = serde_wasm_bindgen::to_value(&K::encode_upper(room_id, &self.inner))?;
        IdbKeyRange::bound(&lower, &upper).map_err(Into::into)
    }

    /// Encodes a bounded key range for an [`Indexed`] type from `lower` to
    /// `upper`.
    ///
    /// Note that the particular key which is encoded is defined by the type
    /// `K`.
    pub fn encode_key_range_from_to<T, K>(
        &self,
        room_id: &RoomId,
        lower: &K::KeyComponents,
        upper: &K::KeyComponents,
    ) -> Result<IdbKeyRange, serde_wasm_bindgen::Error>
    where
        T: Indexed,
        K: IndexedKeyBounds<T> + Serialize,
    {
        let lower = serde_wasm_bindgen::to_value(&K::encode(room_id, lower, &self.inner))?;
        let upper = serde_wasm_bindgen::to_value(&K::encode(room_id, upper, &self.inner))?;
        IdbKeyRange::bound(&lower, &upper).map_err(Into::into)
    }

    /// Serializes an [`Indexed`] type into a [`JsValue`]
    pub fn serialize<T>(
        &self,
        room_id: &RoomId,
        t: &T,
    ) -> Result<JsValue, IndexeddbEventCacheStoreSerializerError>
    where
        T: Indexed,
        T::IndexedType: Serialize,
        T::Error: std::error::Error + 'static,
    {
        let indexed = t
            .to_indexed(room_id, &self.inner)
            .map_err(|e| IndexeddbEventCacheStoreSerializerError::Indexing(Box::new(e)))?;
        serde_wasm_bindgen::to_value(&indexed).map_err(Into::into)
    }

    /// Deserializes an [`Indexed`] type from a [`JsValue`]
    pub fn deserialize<T>(
        &self,
        value: JsValue,
    ) -> Result<T, IndexeddbEventCacheStoreSerializerError>
    where
        T: Indexed,
        T::IndexedType: DeserializeOwned,
        T::Error: std::error::Error + 'static,
    {
        let indexed: T::IndexedType = value.into_serde()?;
        T::from_indexed(indexed, &self.inner)
            .map_err(|e| IndexeddbEventCacheStoreSerializerError::Indexing(Box::new(e)))
    }
}
