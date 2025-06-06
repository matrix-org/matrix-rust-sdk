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

use base64::{
    alphabet,
    engine::{general_purpose, GeneralPurpose},
    Engine,
};
use gloo_utils::format::JsValueSerdeExt;
use matrix_sdk_crypto::CryptoStoreError;
use matrix_sdk_store_encryption::{EncryptedValueBase64, StoreCipher};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;
use zeroize::Zeroizing;

use crate::safe_encode::SafeEncode;

type Result<A, E = IndexeddbSerializerError> = std::result::Result<A, E>;

const BASE64: GeneralPurpose = GeneralPurpose::new(&alphabet::STANDARD, general_purpose::NO_PAD);

/// Handles the functionality of serializing and encrypting data for the
/// indexeddb store.
pub struct IndexeddbSerializer {
    store_cipher: Option<Arc<StoreCipher>>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for IndexeddbSerializer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbSerializer")
            .field("store_cipher", &self.store_cipher.as_ref().map(|_| "<StoreCipher>"))
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbSerializerError {
    #[error(transparent)]
    Serialization(#[from] serde_json::Error),
    #[error("DomException {name} ({code}): {message}")]
    DomException {
        /// DomException code
        code: u16,
        /// Specific name of the DomException
        name: String,
        /// Message given to the DomException
        message: String,
    },
    #[error(transparent)]
    CryptoStoreError(#[from] CryptoStoreError),
}

impl From<web_sys::DomException> for IndexeddbSerializerError {
    fn from(frm: web_sys::DomException) -> Self {
        Self::DomException { name: frm.name(), message: frm.message(), code: frm.code() }
    }
}

impl From<serde_wasm_bindgen::Error> for IndexeddbSerializerError {
    fn from(e: serde_wasm_bindgen::Error) -> Self {
        Self::Serialization(serde::de::Error::custom(e.to_string()))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MaybeEncrypted {
    Encrypted(EncryptedValueBase64),
    Unencrypted(String),
}

impl IndexeddbSerializer {
    pub fn new(store_cipher: Option<Arc<StoreCipher>>) -> Self {
        Self { store_cipher }
    }

    /// Hash the given key securely for the given tablename, using the store
    /// cipher.
    ///
    /// First calls [`SafeEncode::as_encoded_string`]
    /// on the `key` to encode it into a formatted string.
    ///
    /// Then, if a cipher is configured, hashes the formatted key and returns
    /// the hash encoded as unpadded base64.
    ///
    /// If no cipher is configured, just returns the formatted key.
    ///
    /// This is faster than [`Self::serialize_value`] and reliably gives the
    /// same output for the same input, making it suitable for index keys.
    pub fn encode_key<T>(&self, table_name: &str, key: T) -> JsValue
    where
        T: SafeEncode,
    {
        self.encode_key_as_string(table_name, key).into()
    }

    /// Hash the given key securely for the given tablename, using the store
    /// cipher.
    ///
    /// The same as [`Self::encode_key`], but stops short of converting the
    /// resulting base64 string into a JsValue
    pub fn encode_key_as_string<T>(&self, table_name: &str, key: T) -> String
    where
        T: SafeEncode,
    {
        match &self.store_cipher {
            Some(cipher) => key.as_secure_string(table_name, cipher),
            None => key.as_encoded_string(),
        }
    }

    pub fn encode_to_range<T>(
        &self,
        table_name: &str,
        key: T,
    ) -> Result<IdbKeyRange, IndexeddbSerializerError>
    where
        T: SafeEncode,
    {
        match &self.store_cipher {
            Some(cipher) => key.encode_to_range_secure(table_name, cipher),
            None => key.encode_to_range(),
        }
        .map_err(|e| IndexeddbSerializerError::DomException {
            code: 0,
            name: "IdbKeyRangeMakeError".to_owned(),
            message: e,
        })
    }

    /// Encode the value for storage as a value in indexeddb.
    ///
    /// A thin wrapper around [`IndexeddbSerializer::maybe_encrypt_value`]:
    /// encrypts the given object, and then turns the [`MaybeEncrypted`]
    /// result into a JS object for storage in indexeddb.
    pub fn serialize_value(
        &self,
        value: &impl Serialize,
    ) -> Result<JsValue, IndexeddbSerializerError> {
        let serialized = self.maybe_encrypt_value(value)?;
        Ok(serde_wasm_bindgen::to_value(&serialized)?)
    }

    /// Encode the value for storage as a value in indexeddb.
    ///
    /// Returns a byte vector which is either the JSON serialisation of the
    /// value, or an encrypted version thereof.
    ///
    /// Avoid using this in new code. Prefer
    /// [`IndexeddbSerializer::serialize_value`] or
    /// [`IndexeddbSerializer::maybe_encrypt_value`].
    pub fn serialize_value_as_bytes(
        &self,
        value: &impl Serialize,
    ) -> Result<Vec<u8>, CryptoStoreError> {
        match &self.store_cipher {
            Some(cipher) => cipher.encrypt_value(value).map_err(CryptoStoreError::backend),
            None => serde_json::to_vec(value).map_err(CryptoStoreError::backend),
        }
    }

    /// Encode an object for storage as a value in indexeddb.
    ///
    /// First serializes the object as JSON bytes.
    ///
    /// Then, if a cipher is set, encrypts the JSON with a nonce into binary
    /// blobs, and base64-encodes the blobs.
    ///
    /// If no cipher is set, just base64-encodes the JSON bytes.
    ///
    /// Finally, returns an object encapsulating the result.
    pub fn maybe_encrypt_value<T: Serialize>(
        &self,
        value: T,
    ) -> Result<MaybeEncrypted, CryptoStoreError> {
        // First serialize the object as JSON.
        let serialized = serde_json::to_vec(&value).map_err(CryptoStoreError::backend)?;

        // Then either encrypt the JSON, or just base64-encode it.
        Ok(match &self.store_cipher {
            Some(cipher) => MaybeEncrypted::Encrypted(
                cipher.encrypt_value_base64_data(serialized).map_err(CryptoStoreError::backend)?,
            ),
            None => MaybeEncrypted::Unencrypted(BASE64.encode(serialized)),
        })
    }

    /// Decode a value that was previously encoded with
    /// [`Self::serialize_value`].
    pub fn deserialize_value<T: DeserializeOwned>(
        &self,
        value: JsValue,
    ) -> Result<T, IndexeddbSerializerError> {
        // Objects which are serialized nowadays should be represented as a
        // `MaybeEncrypted`. However, `serialize_value` previously used a
        // different format, so we need to handle that in case we have old data.
        //
        // If we can convert the JsValue into a `MaybeEncrypted`, then it's probably one
        // of those.
        //
        // - `MaybeEncrypted::Encrypted` becomes a JS object with properties {`version`,
        //   `nonce`, `ciphertext`}.
        //
        // - `MaybeEncrypted::Unencrypted` becomes a JS string containing base64 text.
        //
        // Otherwise, it probably uses our old serialization format:
        //
        // - Encrypted values were: serialized to an array of JSON bytes; encrypted to
        //   an array of u8 bytes; stored in a Rust object; serialized (again) into an
        //   array of JSON bytes. Net result is a JS array.
        //
        // - Unencrypted values were serialized to JSON, then deserialized into a
        //   javascript object/string/array/bool.
        //
        // Note that there are several potential ambiguities here:
        //
        // - A JS string could either be a legacy unencrypted value, or a
        //   `MaybeEncrypted::Unencrypted`. However, the only thing that actually got
        //   stored as a string under the legacy system was `backup_key_v1`, and that is
        //   special-cased not to use this path — so if we can convert it into a
        //   `MaybeEncrypted::Unencrypted`, then we assume it is one.
        //
        // - A JS array could be either a legacy encrypted value or a legacy unencrypted
        //   value. We can tell the difference by whether we have a `cipher`.
        //
        // - A JS object could be either a legacy unencrypted value or a
        //   `MaybeEncrypted::Encrypted`. We assume that no legacy JS objects have the
        //   properties to be successfully decoded into a `MaybeEncrypted::Encrypted`.

        // First check if it looks like a `MaybeEncrypted`, of either type.
        if let Ok(maybe_encrypted) = serde_wasm_bindgen::from_value(value.clone()) {
            return Ok(self.maybe_decrypt_value(maybe_encrypted)?);
        }

        // Otherwise, fall back to the legacy deserializer.
        self.deserialize_legacy_value(value)
    }

    /// Decode a value that was encoded with an old version of
    /// `serialize_value`.
    ///
    /// This should only be used on values from an old database which are known
    /// to be serialized with the old format.
    pub fn deserialize_legacy_value<T: DeserializeOwned>(
        &self,
        value: JsValue,
    ) -> Result<T, IndexeddbSerializerError> {
        match &self.store_cipher {
            Some(cipher) => {
                if !value.is_array() {
                    return Err(IndexeddbSerializerError::CryptoStoreError(
                        CryptoStoreError::UnpicklingError,
                    ));
                }

                // Looks like legacy encrypted format.
                //
                // `value` is a JS-side array containing the byte values. Turn it into a
                // rust-side Vec<u8>, then decrypt.
                let value: Vec<u8> = serde_wasm_bindgen::from_value(value)?;
                Ok(cipher.decrypt_value(&value).map_err(CryptoStoreError::backend)?)
            }

            None => {
                // Legacy unencrypted format could be just about anything; just try
                // JSON-serializing the value, then deserializing it into the
                // desired type.
                //
                // Note that the stored data was actually encoded by JSON-serializing it, and
                // then deserializing the JSON into Javascript objects — so, for
                // example, `HashMap`s are converted into Javascript Objects
                // (whose keys are always strings) rather than Maps (whose keys
                // can be other things). `serde_wasm_bindgen::from_value` will complain about
                // such things. The correct thing to do is to go *back* to JSON
                // and then deserialize into Rust again, which is what `JsValue::into_serde`
                // does.
                Ok(value.into_serde()?)
            }
        }
    }

    /// Decode a value that was previously encoded with
    /// [`Self::serialize_value_as_bytes`]
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

    /// Decode a value that was previously encoded with
    /// [`Self::maybe_encrypt_value`]
    pub fn maybe_decrypt_value<T: DeserializeOwned>(
        &self,
        value: MaybeEncrypted,
    ) -> Result<T, CryptoStoreError> {
        // First extract the plaintext JSON, either by decrypting or un-base64-ing.
        let plaintext = Zeroizing::new(match (&self.store_cipher, value) {
            (Some(cipher), MaybeEncrypted::Encrypted(enc)) => {
                cipher.decrypt_value_base64_data(enc).map_err(CryptoStoreError::backend)?
            }
            (None, MaybeEncrypted::Unencrypted(unc)) => {
                BASE64.decode(unc).map_err(CryptoStoreError::backend)?
            }

            _ => return Err(CryptoStoreError::UnpicklingError),
        });

        // Then deserialize the JSON.
        Ok(serde_json::from_slice(&plaintext)?)
    }
}

#[cfg(all(test, target_family = "wasm"))]
mod tests {
    use std::{collections::BTreeMap, sync::Arc};

    use gloo_utils::format::JsValueSerdeExt;
    use matrix_sdk_store_encryption::StoreCipher;
    use matrix_sdk_test::async_test;
    use serde::{Deserialize, Serialize};
    use serde_json::json;
    use wasm_bindgen::JsValue;

    use super::IndexeddbSerializer;

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    /// Test that `serialize_value`/`deserialize_value` will round-trip, when a
    /// cipher is in use.
    #[async_test]
    async fn test_serialize_deserialize_with_cipher() {
        let serializer = IndexeddbSerializer::new(Some(Arc::new(StoreCipher::new().unwrap())));

        let obj = make_test_object();
        let serialized = serializer.serialize_value(&obj).expect("could not serialize");
        let deserialized: TestStruct =
            serializer.deserialize_value(serialized).expect("could not deserialize");

        assert_eq!(obj, deserialized);
    }

    /// Test that `serialize_value`/`deserialize_value` will round-trip, when no
    /// cipher is in use.
    #[async_test]
    async fn test_serialize_deserialize_no_cipher() {
        let serializer = IndexeddbSerializer::new(None);
        let obj = make_test_object();
        let serialized = serializer.serialize_value(&obj).expect("could not serialize");
        let deserialized: TestStruct =
            serializer.deserialize_value(serialized).expect("could not deserialize");

        assert_eq!(obj, deserialized);
    }

    /// Test that `deserialize_value` can decode a value that was encoded with
    /// an old implementation of `serialize_value`, when a cipher is in use.
    #[async_test]
    async fn test_deserialize_old_serialized_value_with_cipher() {
        let cipher = test_cipher();
        let obj = make_test_object();

        // Follow the old format for encoding:
        //  1. Encode as JSON, in a Vec<u8> of bytes
        //  2. Encrypt
        //  3. JSON-encode to another Vec<u8>
        //  4. Turn the Vec into a Javascript array of numbers.
        let data = serde_json::to_vec(&obj).unwrap();
        let data = cipher.encrypt_value_data(data).unwrap();
        let data = serde_json::to_vec(&data).unwrap();
        let serialized = JsValue::from_serde(&data).unwrap();

        // Now, try deserializing with `deserialize_value`, and check we get the right
        // thing.
        let serializer = IndexeddbSerializer::new(Some(Arc::new(cipher)));
        let deserialized: TestStruct =
            serializer.deserialize_value(serialized).expect("could not deserialize");

        assert_eq!(obj, deserialized);
    }

    /// Test that `deserialize_value` can decode a value that was encoded with
    /// an old implementation of `serialize_value`, when no cipher is in use.
    #[async_test]
    async fn test_deserialize_old_serialized_value_no_cipher() {
        // An example of an object which was serialized using the old-format
        // `serialize_value`.
        let json = json!({ "id":0, "name": "test", "map": { "0": "test" }});
        let serialized = js_sys::JSON::parse(&json.to_string()).unwrap();

        let serializer = IndexeddbSerializer::new(None);
        let deserialized: TestStruct =
            serializer.deserialize_value(serialized).expect("could not deserialize");

        assert_eq!(make_test_object(), deserialized);
    }

    /// Test that `deserialize_value` can decode an array value that was encoded
    /// with an old implementation of `serialize_value`, when no cipher is
    /// in use.
    #[async_test]
    async fn test_deserialize_old_serialized_array_no_cipher() {
        let json = json!([1, 2, 3, 4]);
        let serialized = js_sys::JSON::parse(&json.to_string()).unwrap();

        let serializer = IndexeddbSerializer::new(None);
        let deserialized: Vec<u8> =
            serializer.deserialize_value(serialized).expect("could not deserialize");

        assert_eq!(vec![1, 2, 3, 4], deserialized);
    }

    /// Test that `deserialize_value` can decode a value encoded with
    /// `maybe_encrypt_value`, when a cipher is in use.
    #[async_test]
    async fn test_maybe_encrypt_deserialize_with_cipher() {
        let serializer = IndexeddbSerializer::new(Some(Arc::new(StoreCipher::new().unwrap())));

        let obj = make_test_object();
        let serialized = serializer.maybe_encrypt_value(&obj).expect("could not serialize");
        let serialized = serde_wasm_bindgen::to_value(&serialized).unwrap();

        let deserialized: TestStruct =
            serializer.deserialize_value(serialized).expect("could not deserialize");

        assert_eq!(obj, deserialized);
    }

    /// Test that `deserialize_value` can decode a value encoded with
    /// `maybe_encrypt_value`, when no cipher is in use.
    #[async_test]
    async fn test_maybe_encrypt_deserialize_no_cipher() {
        let serializer = IndexeddbSerializer::new(None);
        let obj = make_test_object();
        let serialized = serializer.maybe_encrypt_value(&obj).expect("could not serialize");
        let serialized = serde_wasm_bindgen::to_value(&serialized).unwrap();
        let deserialized: TestStruct =
            serializer.deserialize_value(serialized).expect("could not deserialize");

        assert_eq!(obj, deserialized);
    }

    /// Test that `maybe_encrypt_value`/`maybe_decrypt_value` will round-trip,
    /// when a cipher is in use.
    #[async_test]
    async fn test_maybe_encrypt_decrypt_with_cipher() {
        let serializer = IndexeddbSerializer::new(Some(Arc::new(StoreCipher::new().unwrap())));

        let obj = make_test_object();
        let serialized = serializer.maybe_encrypt_value(&obj).expect("could not serialize");
        let deserialized: TestStruct =
            serializer.maybe_decrypt_value(serialized).expect("could not deserialize");

        assert_eq!(obj, deserialized);
    }

    /// Test that `maybe_encrypt_value`/`maybe_decrypt_value` will round-trip,
    /// when no cipher is in use.
    #[async_test]
    async fn test_maybe_encrypt_decrypt_no_cipher() {
        let serializer = IndexeddbSerializer::new(None);

        let obj = make_test_object();
        let serialized = serializer.maybe_encrypt_value(&obj).expect("could not serialize");
        let deserialized: TestStruct =
            serializer.maybe_decrypt_value(serialized).expect("could not deserialize");

        assert_eq!(obj, deserialized);
    }

    #[derive(Serialize, Deserialize, PartialEq, Debug)]
    struct TestStruct {
        id: u32,
        name: String,

        // A map, whose keys are not strings. This is an edge-case we previously got wrong. Maps
        // are represented differently in JSON from Javascript objects, and that particularly
        // matters when their keys are not strings.
        map: BTreeMap<u8, String>,
    }

    fn make_test_object() -> TestStruct {
        TestStruct { id: 0, name: "test".to_owned(), map: BTreeMap::from([(0, "test".to_owned())]) }
    }

    /// Build a [`StoreCipher`] using a hardcoded key.
    fn test_cipher() -> StoreCipher {
        StoreCipher::import_with_key(
            &[0u8; 32],
            &[
                130, 168, 107, 100, 102, 95, 105, 110, 102, 111, 164, 78, 111, 110, 101, 175, 99,
                105, 112, 104, 101, 114, 116, 101, 120, 116, 95, 105, 110, 102, 111, 129, 176, 67,
                104, 97, 67, 104, 97, 50, 48, 80, 111, 108, 121, 49, 51, 48, 53, 130, 165, 110,
                111, 110, 99, 101, 220, 0, 24, 13, 204, 160, 204, 133, 204, 180, 204, 224, 204,
                158, 95, 14, 94, 204, 133, 110, 3, 204, 225, 204, 174, 54, 204, 144, 204, 205, 204,
                190, 204, 155, 74, 118, 81, 87, 204, 156, 170, 99, 105, 112, 104, 101, 114, 116,
                101, 120, 116, 220, 0, 80, 204, 226, 204, 205, 58, 101, 88, 204, 141, 204, 218, 2,
                112, 204, 252, 48, 204, 169, 204, 233, 58, 4, 60, 96, 66, 22, 204, 192, 4, 4, 63,
                109, 204, 157, 204, 166, 17, 55, 85, 102, 89, 204, 145, 110, 204, 250, 39, 18, 19,
                204, 191, 204, 156, 71, 204, 142, 75, 204, 251, 204, 218, 204, 130, 204, 132, 204,
                240, 86, 204, 141, 77, 64, 204, 132, 204, 241, 204, 177, 12, 204, 224, 102, 106, 4,
                204, 141, 89, 101, 30, 45, 38, 105, 104, 204, 156, 96, 204, 203, 204, 224, 34, 125,
                204, 157, 204, 160, 38, 204, 158, 204, 155, 16, 204, 150,
            ],
        )
        .unwrap()
    }
}
