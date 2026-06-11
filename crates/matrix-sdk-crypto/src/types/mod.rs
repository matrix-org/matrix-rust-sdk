// Copyright 2022-2024 The Matrix.org Foundation C.I.C.
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

//! Module containing customized types modeling Matrix keys and events.
//!
//! These types were mostly taken from the Ruma project. The types differ in a
//! couple of important ways to the Ruma types of the same name:
//!
//! 1. They are using vodozemac types so we directly deserialize into a
//!    vodozemac Curve25519 or Ed25519 key.
//! 2. They support lossless serialization cycles in a canonical JSON supported
//!    way, meaning the white-space and field order won't be preserved but the
//!    data will.
//! 3. Types containing secrets implement the [`Zeroize`] and [`ZeroizeOnDrop`]
//!    traits to clear out any memory containing secret key material.

use std::{
    borrow::Borrow,
    collections::{
        BTreeMap,
        btree_map::{IntoIter, Iter},
    },
};

use matrix_sdk_common::deserialized_responses::PrivOwnedStr;
use ruma::{DeviceKeyAlgorithm, DeviceKeyId, OwnedDeviceKeyId, RoomId, serde::StringEnum};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey, KeyError};
use zeroize::{Zeroize, ZeroizeOnDrop};

mod backup;
mod cross_signing;
mod device_keys;
pub mod events;
mod one_time_keys;
pub mod qr_login;
pub mod requests;
pub mod room_history;
mod signatures;

pub use self::{backup::*, cross_signing::*, device_keys::*, one_time_keys::*, signatures::*};
use crate::store::types::BackupDecryptionKey;

macro_rules! from_base64 {
    ($foo:ident, $name:ident) => {
        pub(crate) fn $name<'de, D>(deserializer: D) -> Result<$foo, D::Error>
        where
            D: Deserializer<'de>,
        {
            let mut string = String::deserialize(deserializer)?;

            let result = $foo::from_base64(&string);
            string.zeroize();

            result.map_err(serde::de::Error::custom)
        }
    };
}

macro_rules! to_base64 {
    ($foo:ident, $name:ident) => {
        pub(crate) fn $name<S>(v: &$foo, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            let mut string = v.to_base64();
            let ret = string.serialize(serializer);

            string.zeroize();

            ret
        }
    };
}

/// Struct containing the bundle of secrets to fully activate a new device for
/// end-to-end encryption.
#[derive(Debug, Deserialize, Clone, Serialize, ZeroizeOnDrop)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct SecretsBundle {
    /// The cross-signing keys.
    pub cross_signing: CrossSigningSecrets,
    /// The backup key, if available.
    pub backup: Option<BackupSecrets>,
}

/// Data for the secrets bundle containing the cross-signing keys.
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
#[derive(Deserialize, Clone, Serialize, ZeroizeOnDrop)]
pub struct CrossSigningSecrets {
    /// The seed for the private part of the cross-signing master key, encoded
    /// as base64.
    pub master_key: String,
    /// The seed for the private part of the cross-signing user-signing key,
    /// encoded as base64.
    pub user_signing_key: String,
    /// The seed for the private part of the cross-signing self-signing key,
    /// encoded as base64.
    pub self_signing_key: String,
}

impl std::fmt::Debug for CrossSigningSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossSigningSecrets")
            .field("master_key", &"...")
            .field("user_signing_key", &"...")
            .field("self_signing_key", &"...")
            .finish()
    }
}

/// Data for the secrets bundle containing the secret and version for a
/// `m.megolm_backup.v1.curve25519-aes-sha2` backup.
#[derive(Debug, Deserialize, Clone, Serialize, ZeroizeOnDrop)]
pub struct MegolmBackupV1Curve25519AesSha2Secrets {
    /// The private half of the backup key, can be used to access and decrypt
    /// room keys in the backup. Also called the recovery key in the
    /// [spec](https://spec.matrix.org/v1.10/client-server-api/#recovery-key).
    #[serde(serialize_with = "backup_key_to_base64", deserialize_with = "backup_key_from_base64")]
    pub key: BackupDecryptionKey,
    /// The backup version that is tied to the above backup key.
    pub backup_version: String,
}

from_base64!(BackupDecryptionKey, backup_key_from_base64);
to_base64!(BackupDecryptionKey, backup_key_to_base64);

/// Enum for the algorithm-specific secrets for the room key backup.
#[derive(Debug, Clone, ZeroizeOnDrop, Serialize, Deserialize)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
#[serde(tag = "algorithm")]
pub enum BackupSecrets {
    /// Backup secrets for the `m.megolm_backup.v1.curve25519-aes-sha2` backup
    /// algorithm.
    #[serde(rename = "m.megolm_backup.v1.curve25519-aes-sha2")]
    MegolmBackupV1Curve25519AesSha2(MegolmBackupV1Curve25519AesSha2Secrets),
}

impl BackupSecrets {
    /// Get the algorithm of the secrets contained in the [`BackupSecrets`].
    pub fn algorithm(&self) -> &str {
        match &self {
            BackupSecrets::MegolmBackupV1Curve25519AesSha2(_) => {
                "m.megolm_backup.v1.curve25519-aes-sha2"
            }
        }
    }
}

/// A collection of signing keys, a map from the key id to the signing key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningKeys<T: Ord>(BTreeMap<T, SigningKey>);

impl<T: Ord> SigningKeys<T> {
    /// Create a new, empty, `SigningKeys` collection.
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Insert a `SigningKey` into the collection.
    pub fn insert(&mut self, key_id: T, key: SigningKey) -> Option<SigningKey> {
        self.0.insert(key_id, key)
    }

    /// Get a `SigningKey` with the given `DeviceKeyId`.
    pub fn get<Q>(&self, key_id: &Q) -> Option<&SigningKey>
    where
        T: Borrow<Q>,
        Q: Ord + ?Sized,
    {
        self.0.get(key_id)
    }

    /// Create an iterator over the `SigningKey`s in this collection.
    pub fn iter(&self) -> Iter<'_, T, SigningKey> {
        self.0.iter()
    }

    /// Do we hold any keys in or is our collection completely empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: Ord> Default for SigningKeys<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Ord> IntoIterator for SigningKeys<T> {
    type Item = (T, SigningKey);

    type IntoIter = IntoIter<T, SigningKey>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<K: Ord> FromIterator<(K, SigningKey)> for SigningKeys<K> {
    fn from_iter<T: IntoIterator<Item = (K, SigningKey)>>(iter: T) -> Self {
        let map = BTreeMap::from_iter(iter);

        Self(map)
    }
}

impl<K: Ord, const N: usize> From<[(K, SigningKey); N]> for SigningKeys<K> {
    fn from(v: [(K, SigningKey); N]) -> Self {
        let map = BTreeMap::from(v);

        Self(map)
    }
}

// Helper trait to generalize between a `OwnedDeviceKeyId` and a
// `DeviceKeyAlgorithm` so that we can support Deserialize for
// `SigningKeys<T>`
trait Algorithm {
    fn algorithm(&self) -> DeviceKeyAlgorithm;
}

impl Algorithm for OwnedDeviceKeyId {
    fn algorithm(&self) -> DeviceKeyAlgorithm {
        DeviceKeyId::algorithm(self)
    }
}

impl Algorithm for DeviceKeyAlgorithm {
    fn algorithm(&self) -> DeviceKeyAlgorithm {
        self.to_owned()
    }
}

/// An encryption algorithm to be used to encrypt messages sent to a room.
#[derive(Clone, StringEnum)]
#[non_exhaustive]
pub enum EventEncryptionAlgorithm {
    /// Olm version 1 using Curve25519, AES-256, and SHA-256.
    #[ruma_enum(rename = "m.olm.v1.curve25519-aes-sha2")]
    OlmV1Curve25519AesSha2,

    /// Olm version 2 using Curve25519, AES-256, and SHA-256.
    #[cfg(feature = "experimental-algorithms")]
    #[ruma_enum(rename = "m.olm.v2.curve25519-aes-sha2")]
    OlmV2Curve25519AesSha2,

    /// Megolm version 1 using AES-256 and SHA-256.
    #[ruma_enum(rename = "m.megolm.v1.aes-sha2")]
    MegolmV1AesSha2,

    /// Megolm version 2 using AES-256 and SHA-256.
    #[cfg(feature = "experimental-algorithms")]
    #[ruma_enum(rename = "m.megolm.v2.aes-sha2")]
    MegolmV2AesSha2,

    #[doc(hidden)]
    _Custom(PrivOwnedStr),
}

impl<T: Ord + Serialize> Serialize for SigningKeys<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let keys: BTreeMap<&T, String> =
            self.0.iter().map(|(key_id, key)| (key_id, key.to_base64())).collect();

        keys.serialize(serializer)
    }
}

impl<'de, T: Algorithm + Ord + Deserialize<'de>> Deserialize<'de> for SigningKeys<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let map: BTreeMap<T, String> = Deserialize::deserialize(deserializer)?;

        let map: Result<_, _> = map
            .into_iter()
            .map(|(key_id, key)| {
                let key = SigningKey::from_parts(&key_id.algorithm(), key)
                    .map_err(serde::de::Error::custom)?;

                Ok((key_id, key))
            })
            .collect();

        Ok(SigningKeys(map?))
    }
}

// Vodozemac serializes Curve25519 keys directly as a byteslice, while Matrix
// likes to base64 encode all byte slices.
//
// This ensures that we serialize/deserialize in a Matrix-compatible way.
from_base64!(Curve25519PublicKey, deserialize_curve_key);
to_base64!(Curve25519PublicKey, serialize_curve_key);

from_base64!(Ed25519PublicKey, deserialize_ed25519_key);
to_base64!(Ed25519PublicKey, serialize_ed25519_key);

pub(crate) fn deserialize_curve_key_vec<'de, D>(de: D) -> Result<Vec<Curve25519PublicKey>, D::Error>
where
    D: Deserializer<'de>,
{
    let keys: Vec<String> = Deserialize::deserialize(de)?;
    let keys: Result<Vec<Curve25519PublicKey>, KeyError> =
        keys.iter().map(|k| Curve25519PublicKey::from_base64(k)).collect();

    keys.map_err(serde::de::Error::custom)
}

pub(crate) fn serialize_curve_key_vec<S>(
    keys: &[Curve25519PublicKey],
    s: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let keys: Vec<String> = keys.iter().map(|k| k.to_base64()).collect();
    keys.serialize(s)
}

mod serde_curve_key_option {
    use super::{Curve25519PublicKey, Deserialize, Deserializer, Serialize, Serializer};

    pub(crate) fn deserialize<'de, D>(de: D) -> Result<Option<Curve25519PublicKey>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let key: Option<String> = Deserialize::deserialize(de)?;
        key.map(|k| Curve25519PublicKey::from_base64(&k))
            .transpose()
            .map_err(serde::de::Error::custom)
    }

    pub(crate) fn serialize<S>(key: &Option<Curve25519PublicKey>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let key = key.as_ref().map(|k| k.to_base64());
        key.serialize(s)
    }
}

/// Trait to express the various room key export formats we have in a unified
/// manner.
pub trait RoomKeyExport {
    /// The ID of the room where the exported room key was used.
    fn room_id(&self) -> &RoomId;
    /// The unique ID of the exported room key.
    fn session_id(&self) -> &str;
    /// The [Curve25519PublicKey] long-term identity key of the sender of this
    /// room key.
    fn sender_key(&self) -> Curve25519PublicKey;
}

#[cfg(test)]
mod test {
    use insta::{assert_debug_snapshot, assert_json_snapshot};
    use serde_json::json;
    use similar_asserts::assert_eq;

    use super::*;

    #[test]
    fn serialize_secrets_bundle() {
        let json = json!({
            "cross_signing": {
                "master_key": "rTtSv67XGS6k/rg6/yTG/m573cyFTPFRqluFhQY+hSw",
                "self_signing_key": "4jbPt7jh5D2iyM4U+3IDa+WthgJB87IQN1ATdkau+xk",
                "user_signing_key": "YkFKtkjcsTxF6UAzIIG/l6Nog/G2RigCRfWj3cjNWeM",
            },
            "backup": {
                "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
                "backup_version": "2",
                "key": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
            },
        });

        let deserialized: SecretsBundle = serde_json::from_value(json.clone())
            .expect("We should be able to deserialize the secrets bundle");

        let serialized = serde_json::to_value(&deserialized)
            .expect("We should be able to serialize a secrets bundle");

        assert_eq!(json, serialized, "A serialization cycle should yield the same result");
    }

    #[test]
    fn snapshot_backup_decryption_key() {
        let decryption_key = BackupDecryptionKey { inner: Box::new([1u8; 32]) };
        assert_json_snapshot!(decryption_key);

        // should not log the key !
        assert_debug_snapshot!(decryption_key);
    }

    #[test]
    fn snapshot_secret_bundle() {
        let secret_bundle = SecretsBundle {
            cross_signing: CrossSigningSecrets {
                master_key: "MSKMSKMSKMSKMSKMSKMSKMSKMSKMSKMSKMSK".to_owned(),
                user_signing_key: "USKUSKUSKUSKUSKUSKUSKUSKUSKUSKUSKUSK".to_owned(),
                self_signing_key: "SSKSSKSSKSSKSSKSSKSSKSSKSSKSSKSSK".to_owned(),
            },
            backup: Some(BackupSecrets::MegolmBackupV1Curve25519AesSha2(
                MegolmBackupV1Curve25519AesSha2Secrets {
                    key: BackupDecryptionKey::from_bytes(&[0u8; 32]),
                    backup_version: "v1.1".to_owned(),
                },
            )),
        };

        assert_json_snapshot!(secret_bundle);

        let secret_bundle = SecretsBundle {
            cross_signing: CrossSigningSecrets {
                master_key: "MSKMSKMSKMSKMSKMSKMSKMSKMSKMSKMSKMSK".to_owned(),
                user_signing_key: "USKUSKUSKUSKUSKUSKUSKUSKUSKUSKUSKUSK".to_owned(),
                self_signing_key: "SSKSSKSSKSSKSSKSSKSSKSSKSSKSSKSSK".to_owned(),
            },
            backup: None,
        };

        assert_json_snapshot!(secret_bundle);
    }
}
