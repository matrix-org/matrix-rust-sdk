// Copyright 2021 Devin Ragotzy.
// Copyright 2021 Timo Kösters.
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

use ruma::{
    encryption::KeyUsage, serde::Raw, DeviceKeyAlgorithm, DeviceKeyId, OwnedDeviceKeyId,
    OwnedUserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{value::to_raw_value, Value};
use vodozemac::{Ed25519PublicKey, KeyError};

use super::{Signatures, SigningKeys};

/// A cross signing key.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CrossSigningKey {
    /// The ID of the user the key belongs to.
    pub user_id: OwnedUserId,

    /// What the key is used for.
    pub usage: Vec<KeyUsage>,

    /// The public key.
    ///
    /// The object must have exactly one property.
    pub keys: SigningKeys<OwnedDeviceKeyId>,

    /// Signatures of the key.
    ///
    /// Only optional for master key.
    #[serde(default, skip_serializing_if = "Signatures::is_empty")]
    pub signatures: Signatures,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl CrossSigningKey {
    /// Creates a new `CrossSigningKey` with the given user ID, usage, keys and
    /// signatures.
    pub fn new(
        user_id: OwnedUserId,
        usage: Vec<KeyUsage>,
        keys: SigningKeys<OwnedDeviceKeyId>,
        signatures: Signatures,
    ) -> Self {
        Self { user_id, usage, keys, signatures, other: BTreeMap::new() }
    }

    /// Serialize the cross signing key into a Raw version.
    pub fn to_raw<T>(&self) -> Raw<T> {
        Raw::from_json(to_raw_value(&self).expect("Coulnd't serialize cross signing keys"))
    }

    /// Get the Ed25519 cross-signing key (and its ID).
    ///
    /// Structurally, a cross-signing key could contain more than one actual
    /// key. However, the spec [forbids this][cross_signing_key_spec] (see
    /// the `keys` field description), so we just get the first one.
    ///
    /// [cross_signing_key_spec]: https//spec.matrix.org/v1.2/client-server-api/#post_matrixclientv3keysdevice_signingupload
    pub fn get_first_key_and_id(&self) -> Option<(&DeviceKeyId, Ed25519PublicKey)> {
        self.keys.iter().find_map(|(id, key)| Some((id.as_ref(), key.ed25519()?)))
    }
}

/// An enum over the different key types a cross-signing key can have.
///
/// Currently cross signing keys support an ed25519 keypair. The keys transport
/// format is a base64 encoded string, any unknown key type will be left as such
/// a string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SigningKey {
    /// The ed25519 cross-signing key.
    Ed25519(Ed25519PublicKey),
    /// An unknown cross-signing key.
    Unknown(String),
}

impl SigningKey {
    /// Convert the `SigningKey` into a base64 encoded string.
    pub fn to_base64(&self) -> String {
        match self {
            SigningKey::Ed25519(k) => k.to_base64(),
            SigningKey::Unknown(k) => k.to_owned(),
        }
    }

    /// Get the Ed25519 key, if the cross-signing key is actually an Ed25519
    /// key.
    pub fn ed25519(&self) -> Option<Ed25519PublicKey> {
        if let SigningKey::Ed25519(k) = self {
            Some(*k)
        } else {
            None
        }
    }

    /// Try to create a `SigningKey` from an `DeviceKeyAlgorithm` and a string
    /// containing the base64 encoded public key.
    pub fn from_parts(algorithm: &DeviceKeyAlgorithm, key: String) -> Result<Self, KeyError> {
        match algorithm {
            DeviceKeyAlgorithm::Ed25519 => Ed25519PublicKey::from_base64(&key).map(|k| k.into()),
            _ => Ok(Self::Unknown(key)),
        }
    }
}

impl From<Ed25519PublicKey> for SigningKey {
    fn from(val: Ed25519PublicKey) -> Self {
        SigningKey::Ed25519(val)
    }
}

#[cfg(test)]
mod tests {
    use ruma::user_id;
    use serde_json::json;

    use super::CrossSigningKey;

    #[test]
    fn serialization() {
        let json = json!({
            "user_id": "@example:localhost",
              "usage": [
                "master"
              ],
              "keys": {
                "ed25519:rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0": "rJ2TAGkEOP6dX41Ksll6cl8K3J48l8s/59zaXyvl2p0"
              },
              "signatures": {
                "@example:localhost": {
                  "ed25519:WSKKLTJZCL": "ZzJp1wtmRdykXAUEItEjNiFlBrxx8L6/Vaen9am8AuGwlxxJtOkuY4m+4MPLvDPOgavKHLsrRuNLAfCeakMlCQ"
                }
              },
              "other_data": "other"
        });

        let key: CrossSigningKey =
            serde_json::from_value(json.clone()).expect("Can't deserialize cross signing key");

        assert_eq!(key.user_id, user_id!("@example:localhost"));

        let serialized = serde_json::to_value(key).expect("Can't reserialize cross signing key");

        assert_eq!(json, serialized);
    }
}
