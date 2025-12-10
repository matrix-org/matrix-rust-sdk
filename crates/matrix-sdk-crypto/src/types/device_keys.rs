// Copyright 2020 Karl Linderhed.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:

// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

use std::collections::BTreeMap;

use js_option::JsOption;
use ruma::{
    DeviceKeyAlgorithm, DeviceKeyId, OwnedDeviceId, OwnedDeviceKeyId, OwnedUserId,
    serde::{JsonCastable, Raw},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::to_raw_value};
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

use super::{EventEncryptionAlgorithm, Signatures};
use crate::{
    SignatureError,
    olm::{SignedJsonObject, VerifyJson},
};

/// Represents a Matrix cryptographic device
///
/// This struct models a Matrix cryptographic device involved in end-to-end
/// encrypted messaging, specifically for to-device communication. It aligns
/// with the [`DeviceKeys` struct][device_keys_spec] in the Matrix
/// Specification, encapsulating essential elements such as the public device
/// identity keys.
///
/// See also [`ruma::encryption::DeviceKeys`] which is similar, but slightly
/// less comprehensive (it lacks some fields, and  the `keys` are represented as
/// base64 strings rather than type-safe [`DeviceKey`]s). We always use this
/// struct to build `/keys/upload` requests and to deserialize `/keys/query`
/// responses.
///
/// [device_keys_spec]: https://spec.matrix.org/v1.10/client-server-api/#_matrixclientv3keysupload_devicekeys
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(try_from = "DeviceKeyHelper", into = "DeviceKeyHelper")]
pub struct DeviceKeys {
    /// The ID of the user the device belongs to.
    ///
    /// Must match the user ID used when logging in.
    pub user_id: OwnedUserId,

    /// The ID of the device these keys belong to.
    ///
    /// Must match the device ID used when logging in.
    pub device_id: OwnedDeviceId,

    /// The encryption algorithms supported by this device.
    pub algorithms: Vec<EventEncryptionAlgorithm>,

    /// Public identity keys.
    pub keys: BTreeMap<OwnedDeviceKeyId, DeviceKey>,

    /// Signatures for the device key object.
    pub signatures: Signatures,

    /// Whether the device is a dehydrated device or not
    #[serde(default, skip_serializing_if = "JsOption::is_undefined")]
    pub dehydrated: JsOption<bool>,

    /// Additional data added to the device key information by intermediate
    /// servers, and not covered by the signatures.
    #[serde(default, skip_serializing_if = "UnsignedDeviceInfo::is_empty")]
    pub unsigned: UnsignedDeviceInfo,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl DeviceKeys {
    /// Creates a new `DeviceKeys` from the given user id, device ID,
    /// algorithms, keys and signatures.
    pub fn new(
        user_id: OwnedUserId,
        device_id: OwnedDeviceId,
        algorithms: Vec<EventEncryptionAlgorithm>,
        keys: BTreeMap<OwnedDeviceKeyId, DeviceKey>,
        signatures: Signatures,
    ) -> Self {
        Self {
            user_id,
            device_id,
            algorithms,
            keys,
            signatures,
            dehydrated: JsOption::Undefined,
            unsigned: Default::default(),
            other: BTreeMap::new(),
        }
    }

    /// Serialize the device keys key into a Raw version.
    pub fn to_raw<T>(&self) -> Raw<T> {
        Raw::from_json(to_raw_value(&self).expect("Couldn't serialize device keys"))
    }

    /// Get the key of the given key algorithm belonging to this device.
    pub fn get_key(&self, algorithm: DeviceKeyAlgorithm) -> Option<&DeviceKey> {
        self.keys.get(&DeviceKeyId::from_parts(algorithm, &self.device_id))
    }

    /// Get the Curve25519 key of the given device.
    pub fn curve25519_key(&self) -> Option<Curve25519PublicKey> {
        self.get_key(DeviceKeyAlgorithm::Curve25519)
            .and_then(|k| if let DeviceKey::Curve25519(k) = k { Some(*k) } else { None })
    }

    /// Get the Ed25519 key of the given device.
    pub fn ed25519_key(&self) -> Option<Ed25519PublicKey> {
        self.get_key(DeviceKeyAlgorithm::Ed25519)
            .and_then(|k| if let DeviceKey::Ed25519(k) = k { Some(*k) } else { None })
    }

    /// Verify that the given object has been signed by the Ed25519 key in this
    /// `DeviceKeys`.
    pub fn has_signed(&self, signed_object: &impl SignedJsonObject) -> Result<(), SignatureError> {
        let key = self.ed25519_key().ok_or(SignatureError::MissingSigningKey)?;
        let key_id = &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.device_id);
        key.verify_json(&self.user_id, key_id, signed_object)
    }

    /// Verify that this `DeviceKeys` structure contains a correct
    /// self-signature.
    pub fn check_self_signature(&self) -> Result<(), SignatureError> {
        self.has_signed(self)
    }
}

impl JsonCastable<DeviceKeys> for ruma::encryption::DeviceKeys {}

/// Additional data added to device key information by intermediate servers.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct UnsignedDeviceInfo {
    /// The display name which the user set on the device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_display_name: Option<String>,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl UnsignedDeviceInfo {
    /// Creates an empty `UnsignedDeviceInfo`.
    pub fn new() -> Self {
        Default::default()
    }

    /// Checks whether all fields are empty / `None`.
    pub fn is_empty(&self) -> bool {
        self.device_display_name.is_none()
    }
}

/// An enum over the different key types a device can have.
///
/// Currently devices have a curve25519 and ed25519 keypair. The keys transport
/// format is a base64 encoded string, any unknown key type will be left as such
/// a string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeviceKey {
    /// The curve25519 device key.
    Curve25519(Curve25519PublicKey),
    /// The ed25519 device key.
    Ed25519(Ed25519PublicKey),
    /// An unknown device key.
    Unknown(String),
}

impl DeviceKey {
    /// Convert the `DeviceKey` into a base64 encoded string.
    pub fn to_base64(&self) -> String {
        match self {
            DeviceKey::Curve25519(k) => k.to_base64(),
            DeviceKey::Ed25519(k) => k.to_base64(),
            DeviceKey::Unknown(k) => k.to_owned(),
        }
    }
}

impl From<Curve25519PublicKey> for DeviceKey {
    fn from(val: Curve25519PublicKey) -> Self {
        DeviceKey::Curve25519(val)
    }
}

impl From<Ed25519PublicKey> for DeviceKey {
    fn from(val: Ed25519PublicKey) -> Self {
        DeviceKey::Ed25519(val)
    }
}

/// A de/serialization helper for [`DeviceKeys`] which maps the `keys` to/from
/// [`DeviceKey`]s.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct DeviceKeyHelper {
    pub user_id: OwnedUserId,
    pub device_id: OwnedDeviceId,
    pub algorithms: Vec<EventEncryptionAlgorithm>,
    pub keys: BTreeMap<OwnedDeviceKeyId, String>,
    #[serde(default, skip_serializing_if = "JsOption::is_undefined")]
    pub dehydrated: JsOption<bool>,
    pub signatures: Signatures,
    #[serde(default, skip_serializing_if = "UnsignedDeviceInfo::is_empty")]
    pub unsigned: UnsignedDeviceInfo,
    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl TryFrom<DeviceKeyHelper> for DeviceKeys {
    type Error = vodozemac::KeyError;

    fn try_from(value: DeviceKeyHelper) -> Result<Self, Self::Error> {
        let keys: Result<BTreeMap<OwnedDeviceKeyId, DeviceKey>, vodozemac::KeyError> = value
            .keys
            .into_iter()
            .map(|(k, v)| {
                let key = match k.algorithm() {
                    DeviceKeyAlgorithm::Ed25519 => {
                        DeviceKey::Ed25519(Ed25519PublicKey::from_base64(&v)?)
                    }
                    DeviceKeyAlgorithm::Curve25519 => {
                        DeviceKey::Curve25519(Curve25519PublicKey::from_base64(&v)?)
                    }
                    _ => DeviceKey::Unknown(v),
                };

                Ok((k, key))
            })
            .collect();

        Ok(Self {
            user_id: value.user_id,
            device_id: value.device_id,
            algorithms: value.algorithms,
            keys: keys?,
            dehydrated: value.dehydrated,
            signatures: value.signatures,
            unsigned: value.unsigned,
            other: value.other,
        })
    }
}

impl From<DeviceKeys> for DeviceKeyHelper {
    fn from(value: DeviceKeys) -> Self {
        let keys: BTreeMap<OwnedDeviceKeyId, String> =
            value.keys.into_iter().map(|(k, v)| (k, v.to_base64())).collect();

        Self {
            user_id: value.user_id,
            device_id: value.device_id,
            algorithms: value.algorithms,
            keys,
            dehydrated: value.dehydrated,
            signatures: value.signatures,
            unsigned: value.unsigned,
            other: value.other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use ruma::{OwnedDeviceKeyId, device_id, user_id};
    use serde_json::json;
    use vodozemac::{Curve25519PublicKey, Curve25519SecretKey};

    use super::DeviceKeys;

    #[test]
    fn serialization() {
        let json = json!({
          "algorithms": vec![
              "m.olm.v1.curve25519-aes-sha2",
              "m.megolm.v1.aes-sha2"
          ],
          "device_id": "BNYQQWUMXO",
          "user_id": "@example:localhost",
          "keys": {
              "curve25519:BNYQQWUMXO": "xfgbLIC5WAl1OIkpOzoxpCe8FsRDT6nch7NQsOb15nc",
              "ed25519:BNYQQWUMXO": "2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4"
          },
          "signatures": {
              "@example:localhost": {
                  "ed25519:BNYQQWUMXO": "kTwMrbsLJJM/uFGOj/oqlCaRuw7i9p/6eGrTlXjo8UJMCFAetoyWzoMcF35vSe4S6FTx8RJmqX6rM7ep53MHDQ"
              }
          },
          "unsigned": {
              "device_display_name": "Alice's mobile phone",
              "other_data": "other_value"
          },

          "other_data": "other_value"
        });

        let device_keys: DeviceKeys =
            serde_json::from_value(json.clone()).expect("Can't deserialize device keys");

        assert_eq!(device_keys.user_id, user_id!("@example:localhost"));
        assert_eq!(&device_keys.device_id, device_id!("BNYQQWUMXO"));

        let serialized = serde_json::to_value(device_keys).expect("Can't reserialize device keys");

        assert_eq!(json, serialized);
    }

    #[test]
    fn test_check_self_signature() {
        // A correctly-signed set of keys
        let mut device_keys: DeviceKeys = serde_json::from_value(json!({
          "algorithms": vec![
              "m.olm.v1.curve25519-aes-sha2",
              "m.megolm.v1.aes-sha2"
          ],
          "device_id": "BNYQQWUMXO",
          "user_id": "@example:localhost",
          "keys": {
              "curve25519:BNYQQWUMXO": "xfgbLIC5WAl1OIkpOzoxpCe8FsRDT6nch7NQsOb15nc",
              "ed25519:BNYQQWUMXO": "2/5LWJMow5zhJqakV88SIc7q/1pa8fmkfgAzx72w9G4"
          },
          "signatures": {
              "@example:localhost": {
                  "ed25519:BNYQQWUMXO": "kTwMrbsLJJM/uFGOj/oqlCaRuw7i9p/6eGrTlXjo8UJMCFAetoyWzoMcF35vSe4S6FTx8RJmqX6rM7ep53MHDQ"
              }
          },
          "unsigned": {
              "device_display_name": "Alice's mobile phone"
          }
        })).expect("Can't deserialize device keys");

        device_keys.check_self_signature().expect("Self-signature check failed");

        // Change one of the fields and verify that the signature check fails.
        let new_curve_key = Curve25519SecretKey::new();
        let key_id = OwnedDeviceKeyId::from_str("curve25519:BNYQQWUMXO").unwrap();
        device_keys.keys.insert(key_id, Curve25519PublicKey::from(&new_curve_key).into());

        assert!(device_keys.check_self_signature().is_err());
    }
}
