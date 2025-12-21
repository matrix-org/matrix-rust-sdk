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

use std::collections::BTreeMap;

use ruma::serde::JsonCastable;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use vodozemac::Curve25519PublicKey;

use super::{Signatures, deserialize_curve_key, serialize_curve_key};

/// Auth data for the `m.megolm_backup.v1.curve25519-aes-sha2` backup algorithm
/// as defined in the [spec].
///
/// [spec]: https://spec.matrix.org/unstable/client-server-api/#backup-algorithm-mmegolm_backupv1curve25519-aes-sha2
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MegolmV1AuthData {
    ///The Curve25519 public key used to encrypt the backups.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    pub public_key: Curve25519PublicKey,
    /// *Optional.* Signatures of the auth_data, as Signed JSON.
    #[serde(default)]
    pub signatures: Signatures,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl MegolmV1AuthData {
    // Create a new [`MegolmV1AuthData`] from a public Curve25519 key and a
    // [`Signatures`] map.
    pub(crate) fn new(public_key: Curve25519PublicKey, signatures: Signatures) -> Self {
        Self { public_key, signatures, extra: Default::default() }
    }
}

/// Information pertaining to a room key backup. Can be used to upload a new
/// backup version as defined in the [spec].
///
/// [spec]: https://spec.matrix.org/unstable/client-server-api/#post_matrixclientv3room_keysversion
#[derive(Clone, Debug, Deserialize)]
#[serde(try_from = "BackupInfoHelper")]
pub enum RoomKeyBackupInfo {
    /// The `m.megolm_backup.v1.curve25519-aes-sha2` variant of a backup.
    MegolmBackupV1Curve25519AesSha2(MegolmV1AuthData),
    /// Any other unknown backup variant.
    Other {
        /// The algorithm of the unknown backup variant.
        algorithm: String,
        /// The auth data of the unknown backup variant.
        auth_data: BTreeMap<String, Value>,
    },
}

impl JsonCastable<ruma::api::client::backup::BackupAlgorithm> for RoomKeyBackupInfo {}

impl JsonCastable<RoomKeyBackupInfo> for ruma::api::client::backup::BackupAlgorithm {}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct BackupInfoHelper {
    algorithm: String,
    auth_data: Value,
}

impl TryFrom<BackupInfoHelper> for RoomKeyBackupInfo {
    type Error = serde_json::Error;

    fn try_from(value: BackupInfoHelper) -> Result<Self, Self::Error> {
        Ok(match value.algorithm.as_str() {
            "m.megolm_backup.v1.curve25519-aes-sha2" => {
                let data: MegolmV1AuthData = serde_json::from_value(value.auth_data)?;
                RoomKeyBackupInfo::MegolmBackupV1Curve25519AesSha2(data)
            }
            _ => RoomKeyBackupInfo::Other {
                algorithm: value.algorithm,
                auth_data: serde_json::from_value(value.auth_data)?,
            },
        })
    }
}

impl Serialize for RoomKeyBackupInfo {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let helper = match self {
            RoomKeyBackupInfo::MegolmBackupV1Curve25519AesSha2(d) => BackupInfoHelper {
                algorithm: "m.megolm_backup.v1.curve25519-aes-sha2".to_owned(),
                auth_data: serde_json::to_value(d).map_err(serde::ser::Error::custom)?,
            },
            RoomKeyBackupInfo::Other { algorithm, auth_data } => BackupInfoHelper {
                algorithm: algorithm.to_owned(),
                auth_data: serde_json::to_value(auth_data.clone())
                    .map_err(serde::ser::Error::custom)?,
            },
        };

        helper.serialize(serializer)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches::assert_matches;
    use insta::{assert_json_snapshot, with_settings};
    use ruma::{DeviceKeyAlgorithm, KeyId, user_id};
    use serde_json::{Value, json};
    use vodozemac::{Curve25519PublicKey, Ed25519Signature};

    use super::RoomKeyBackupInfo;
    use crate::types::{MegolmV1AuthData, Signature, Signatures};

    #[test]
    fn serialization() {
        let json = json!({
            "algorithm": "m.megolm_backup.v2",
            "auth_data": {
                "some": "data"
            }
        });

        let deserialized: RoomKeyBackupInfo = serde_json::from_value(json.clone()).unwrap();
        assert_matches!(deserialized, RoomKeyBackupInfo::Other { algorithm: _, auth_data: _ });

        let serialized = serde_json::to_value(deserialized).unwrap();
        assert_eq!(json, serialized);

        let json = json!({
            "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": {
                "public_key":"XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM",
                "signatures": {
                    "@alice:example.org": {
                        "ed25519:deviceid": "signature"
                    }
                }
            }
        });

        let deserialized: RoomKeyBackupInfo = serde_json::from_value(json.clone()).unwrap();
        assert_matches!(deserialized, RoomKeyBackupInfo::MegolmBackupV1Curve25519AesSha2(_));

        let serialized = serde_json::to_value(deserialized).unwrap();
        assert_eq!(json, serialized);
    }

    #[test]
    fn snapshot_room_key_backup_info() {
        let info = RoomKeyBackupInfo::MegolmBackupV1Curve25519AesSha2(MegolmV1AuthData {
            public_key: Curve25519PublicKey::from_bytes([2u8; 32]),
            signatures: Signatures(BTreeMap::from([(
                user_id!("@alice:localhost").to_owned(),
                BTreeMap::from([(
                    KeyId::from_parts(DeviceKeyAlgorithm::Ed25519, "ABCDEFG".into()),
                    Ok(Signature::from(Ed25519Signature::from_slice(&[0u8; 64]).unwrap())),
                )]),
            )])),
            extra: BTreeMap::from([("foo".to_owned(), Value::from("bar"))]),
        });

        with_settings!({sort_maps =>true}, {
            assert_json_snapshot!(info)
        });

        let info = RoomKeyBackupInfo::Other {
            algorithm: "caesar.cipher".to_owned(),
            auth_data: BTreeMap::from([("foo".to_owned(), Value::from("bar"))]),
        };

        with_settings!({sort_maps =>true}, {
            assert_json_snapshot!(info);
        })
    }
}
