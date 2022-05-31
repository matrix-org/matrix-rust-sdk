// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! Module containing customized types modeling Matrix keys.
//!
//! These types were mostly taken from the Ruma project. The types differ in two
//! important ways to the Ruma types of the same name:
//!
//! 1. They are using vodozemac types so we directly deserialize into a
//!    vodozemac curve25519 or ed25519 key.
//! 2. They support lossless serialization cycles in a canonical JSON supported
//!    way, meaning the white-space and field order won't be preserved but the
//!    data will.

mod backup;
mod cross_signing_key;
mod device_keys;
mod one_time_keys;

use std::collections::BTreeMap;

pub use backup::*;
pub use cross_signing_key::*;
pub use device_keys::*;
pub use one_time_keys::*;
use ruma::{DeviceKeyAlgorithm, DeviceKeyId, OwnedDeviceKeyId, OwnedUserId, UserId};
use serde::{Deserialize, Serialize, Serializer};
use vodozemac::{Curve25519PublicKey, Ed25519Signature};

/// Represents a potentially decoded signature (but *not* a validated one).
///
/// There are two important cases here:
///
/// 1. If the claimed algorithm is supported *and* the payload has an expected
///    format, the signature will be represent by the enum variant corresponding
///    to that algorithm. For example, decodeable Ed25519 signatures are
///    represented as `Ed25519(...)`.
/// 2. If the claimed algorithm is unsupported, the signature is represented as
///    `Other(...)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signature {
    /// A Ed25519 digital signature.
    Ed25519(Ed25519Signature),
    /// A digital signature in an unsupported algorithm. The raw signature bytes
    /// are represented as a base64-encoded string.
    Other(String),
}

/// Represents a signature that could not be decoded.
///
/// This will currently only hold invalid Ed25519 signatures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSignature {
    /// The base64 encoded string that is claimed to contain a signature but
    /// could not be decoded.
    pub source: String,
}

impl Signature {
    /// Get the Ed25519 signature, if this is one.
    pub fn ed25519(&self) -> Option<Ed25519Signature> {
        if let Self::Ed25519(signature) = &self {
            Some(*signature)
        } else {
            None
        }
    }

    /// Convert the signature to a base64 encoded string.
    pub fn to_base64(&self) -> String {
        match self {
            Signature::Ed25519(s) => s.to_base64(),
            Signature::Other(s) => s.to_owned(),
        }
    }
}

impl From<Ed25519Signature> for Signature {
    fn from(signature: Ed25519Signature) -> Self {
        Self::Ed25519(signature)
    }
}

/// Signatures for a signed object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signatures(
    BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceKeyId, Result<Signature, InvalidSignature>>>,
);

impl Signatures {
    /// Create a new, empty, signatures collection.
    pub fn new() -> Self {
        Signatures(Default::default())
    }

    /// Add the given signature from the given signer and the given key_id to
    /// the collection.
    pub fn add_signature(
        &mut self,
        signer: OwnedUserId,
        key_id: OwnedDeviceKeyId,
        signature: Ed25519Signature,
    ) -> Option<Result<Signature, InvalidSignature>> {
        self.0.entry(signer).or_insert_with(Default::default).insert(key_id, Ok(signature.into()))
    }

    /// Try to find an Ed25519 signature from the given signer with the given
    /// key id.
    pub fn get_signature(&self, signer: &UserId, key_id: &DeviceKeyId) -> Option<Ed25519Signature> {
        self.get(signer)?.get(key_id)?.as_ref().ok()?.ed25519()
    }

    /// Get the map of signatures that belong to the given user.
    pub fn get(
        &self,
        signer: &UserId,
    ) -> Option<&BTreeMap<OwnedDeviceKeyId, Result<Signature, InvalidSignature>>> {
        self.0.get(signer)
    }

    /// Remove all the signatures we currently hold.
    pub fn clear(&mut self) {
        self.0.clear()
    }

    /// Do we hold any signatures or is our collection completely empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many signatures do we currently hold.
    pub fn signature_count(&self) -> usize {
        self.0.iter().map(|(_, u)| u.len()).sum()
    }
}

impl Default for Signatures {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoIterator for Signatures {
    type Item = (OwnedUserId, BTreeMap<OwnedDeviceKeyId, Result<Signature, InvalidSignature>>);

    type IntoIter = std::collections::btree_map::IntoIter<
        OwnedUserId,
        BTreeMap<OwnedDeviceKeyId, Result<Signature, InvalidSignature>>,
    >;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'de> Deserialize<'de> for Signatures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let map: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceKeyId, String>> =
            serde::Deserialize::deserialize(deserializer)?;

        let map = map
            .into_iter()
            .map(|(user, signatures)| {
                let signatures = signatures
                    .into_iter()
                    .map(|(key_id, s)| {
                        let algorithm = key_id.algorithm();
                        let signature = match algorithm {
                            DeviceKeyAlgorithm::Ed25519 => Ed25519Signature::from_base64(&s)
                                .map(|s| s.into())
                                .map_err(|_| InvalidSignature { source: s }),
                            _ => Ok(Signature::Other(s)),
                        };

                        Ok((key_id, signature))
                    })
                    .collect::<Result<BTreeMap<_, _>, _>>()?;

                Ok((user, signatures))
            })
            .collect::<Result<_, _>>()?;

        Ok(Signatures(map))
    }
}

impl Serialize for Signatures {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let signatures: BTreeMap<&OwnedUserId, BTreeMap<&OwnedDeviceKeyId, String>> = self
            .0
            .iter()
            .map(|(u, m)| {
                (
                    u,
                    m.iter()
                        .map(|(d, s)| {
                            (
                                d,
                                match s {
                                    Ok(s) => s.to_base64(),
                                    Err(i) => i.source.to_owned(),
                                },
                            )
                        })
                        .collect(),
                )
            })
            .collect();

        serde::Serialize::serialize(&signatures, serializer)
    }
}

// Vodozemac serializes curve keys directly as a byteslice, while matrix likes
// to base64 encode all byte slices.
//
// This ensures that we serialize/deserialize in a Matrix compatible way.
fn deserialize_curve_key<'de, D>(de: D) -> Result<Curve25519PublicKey, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let key: String = Deserialize::deserialize(de)?;
    Curve25519PublicKey::from_base64(&key).map_err(serde::de::Error::custom)
}

fn serialize_curve_key<S>(key: &Curve25519PublicKey, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let key = key.to_base64();
    s.serialize_str(&key)
}
