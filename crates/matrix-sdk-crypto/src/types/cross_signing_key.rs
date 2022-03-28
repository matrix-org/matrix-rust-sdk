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

use ruma::{encryption::KeyUsage, serde::Raw, DeviceKeyId, UserId};
use serde::{Deserialize, Serialize};
use serde_json::{value::to_raw_value, Value};

/// Signatures for a `CrossSigningKey` object.
pub type CrossSigningKeySignatures = BTreeMap<Box<UserId>, BTreeMap<Box<DeviceKeyId>, String>>;

/// A cross signing key.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CrossSigningKey {
    /// The ID of the user the key belongs to.
    pub user_id: Box<UserId>,

    /// What the key is used for.
    pub usage: Vec<KeyUsage>,

    /// The public key.
    ///
    /// The object must have exactly one property.
    pub keys: BTreeMap<Box<DeviceKeyId>, String>,

    /// Signatures of the key.
    ///
    /// Only optional for master key.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub signatures: CrossSigningKeySignatures,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

impl CrossSigningKey {
    /// Creates a new `CrossSigningKey` with the given user ID, usage, keys and
    /// signatures.
    pub fn new(
        user_id: Box<UserId>,
        usage: Vec<KeyUsage>,
        keys: BTreeMap<Box<DeviceKeyId>, String>,
        signatures: CrossSigningKeySignatures,
    ) -> Self {
        Self { user_id, usage, keys, signatures, other: BTreeMap::new() }
    }

    /// Serialize the cross signing key into a Raw version.
    pub fn to_raw<T>(&self) -> Raw<T> {
        Raw::from_json(to_raw_value(&self).expect("Coulnd't serialize cross signing keys"))
    }
}

#[cfg(test)]
mod test {
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
