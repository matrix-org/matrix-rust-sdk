/*
Copyright 2022-2026 The Matrix.org Foundation C.I.C.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

mod signature;

use std::collections::{BTreeMap, btree_map::IntoIter};

use ruma::{DeviceKeyId, OwnedDeviceKeyId, OwnedUserId, UserId};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use vodozemac::Ed25519Signature;

pub use self::signature::Signature;

/// Represents a signature that could not be decoded.
///
/// This will currently only hold invalid Ed25519 signatures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSignature {
    /// The base64 encoded string that is claimed to contain a signature but
    /// could not be decoded.
    pub source: String,
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
        signature: impl Into<Signature>,
    ) -> Option<Result<Signature, InvalidSignature>> {
        self.0.entry(signer).or_default().insert(key_id, Ok(signature.into()))
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
        self.0.values().map(|u| u.len()).sum()
    }
}

impl Default for Signatures {
    fn default() -> Self {
        Self::new()
    }
}

impl IntoIterator for Signatures {
    type Item = (OwnedUserId, BTreeMap<OwnedDeviceKeyId, Result<Signature, InvalidSignature>>);

    type IntoIter =
        IntoIter<OwnedUserId, BTreeMap<OwnedDeviceKeyId, Result<Signature, InvalidSignature>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'de> Deserialize<'de> for Signatures {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let map: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceKeyId, String>> =
            Deserialize::deserialize(deserializer)?;

        let map = map
            .into_iter()
            .map(|(user, signatures)| {
                let signatures = signatures
                    .into_iter()
                    .map(|(key_id, s)| {
                        let algorithm = key_id.algorithm();
                        let signature = Signature::from_base64(algorithm, s);
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

        Serialize::serialize(&signatures, serializer)
    }
}

#[cfg(test)]
mod test {
    use insta::{assert_json_snapshot, with_settings};
    use ruma::{DeviceKeyAlgorithm, device_id, owned_user_id};

    use super::*;

    #[test]
    fn snapshot_signatures() {
        let signatures = Signatures(BTreeMap::from([
            (
                owned_user_id!("@alice:localhost"),
                BTreeMap::from([
                    (
                        DeviceKeyId::from_parts(
                            DeviceKeyAlgorithm::Ed25519,
                            device_id!("ABCDEFGH"),
                        ),
                        Ok(Signature::from(Ed25519Signature::from_slice(&[0u8; 64]).unwrap())),
                    ),
                    (
                        DeviceKeyId::from_parts(
                            DeviceKeyAlgorithm::Curve25519,
                            device_id!("IJKLMNOP"),
                        ),
                        Ok(Signature::from(Ed25519Signature::from_slice(&[1u8; 64]).unwrap())),
                    ),
                ]),
            ),
            (
                owned_user_id!("@bob:localhost"),
                BTreeMap::from([(
                    DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, device_id!("ABCDEFGH")),
                    Err(InvalidSignature { source: "SOME+B64+SOME+B64+SOME+B64+==".to_owned() }),
                )]),
            ),
        ]));

        with_settings!({sort_maps => true, prepend_module_to_snapshot => false}, {
            assert_json_snapshot!(signatures)
        });
    }
}
