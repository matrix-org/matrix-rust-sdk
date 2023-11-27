// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::sync::Arc;

use gloo_utils::format::JsValueSerdeExt;
use matrix_sdk_crypto::CryptoStoreError;
use matrix_sdk_store_encryption::StoreCipher;
use serde::{de::DeserializeOwned, Serialize};
use wasm_bindgen::JsValue;

use crate::IndexeddbCryptoStoreError;

type Result<A, E = IndexeddbCryptoStoreError> = std::result::Result<A, E>;

/// Handles the functionality of serializing and encrypting data for the
/// indexeddb store.
pub struct IndexeddbSerializer {
    store_cipher: Option<Arc<StoreCipher>>,
}

impl IndexeddbSerializer {
    pub fn new(store_cipher: Option<Arc<StoreCipher>>) -> Self {
        Self { store_cipher }
    }

    /// Encode the value for storage as a value in indexeddb.
    ///
    /// First, serialise the given value as JSON.
    ///
    /// Then, if a store cipher is enabled, encrypt the JSON string using the
    /// configured store cipher, giving a byte array. Then, wrap the byte
    /// array as a `JsValue`.
    ///
    /// If no cipher is enabled, deserialises the JSON string again giving a JS
    /// object.
    pub fn serialize_value(&self, value: &impl Serialize) -> Result<JsValue, CryptoStoreError> {
        if let Some(cipher) = &self.store_cipher {
            let value = cipher.encrypt_value(value).map_err(CryptoStoreError::backend)?;

            Ok(JsValue::from_serde(&value)?)
        } else {
            Ok(JsValue::from_serde(&value)?)
        }
    }

    /// Encode the value for storage as a value in indexeddb.
    ///
    /// This is the same algorithm as [`serialize_value`], but stops short of
    /// encoding the resultant byte vector in a JsValue.
    ///
    /// Returns a byte vector which is either the JSON serialisation of the
    /// value, or an encrypted version thereof.
    pub fn serialize_value_as_bytes(
        &self,
        value: &impl Serialize,
    ) -> Result<Vec<u8>, CryptoStoreError> {
        match &self.store_cipher {
            Some(cipher) => cipher.encrypt_value(value).map_err(CryptoStoreError::backend),
            None => serde_json::to_vec(value).map_err(CryptoStoreError::backend),
        }
    }

    /// Decode a value that was previously encoded with [`serialize_value`]
    pub fn deserialize_value<T: DeserializeOwned>(
        &self,
        value: JsValue,
    ) -> Result<T, CryptoStoreError> {
        if let Some(cipher) = &self.store_cipher {
            let value: Vec<u8> = value.into_serde()?;
            cipher.decrypt_value(&value).map_err(CryptoStoreError::backend)
        } else {
            Ok(value.into_serde()?)
        }
    }

    /// Decode a value that was previously encoded with
    /// [`serialize_value_as_bytes`]
    pub fn deserialize_value_from_bytes<T: DeserializeOwned>(
        &self,
        value: &[u8],
    ) -> Result<T, CryptoStoreError> {
        if let Some(cipher) = &self.store_cipher {
            cipher.decrypt_value(value).map_err(CryptoStoreError::backend)
        } else {
            serde_json::from_slice(value).map_err(CryptoStoreError::backend)
        }
    }
}
