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

use ruma::{OneTimeKeyAlgorithm, serde::Raw};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, value::to_raw_value};
use vodozemac::Curve25519PublicKey;

use super::{Signatures, deserialize_curve_key, serialize_curve_key};

/// A key for the SignedCurve25519 algorithm
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignedKey {
    /// The Curve25519 key that can be used to establish Olm sessions.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    key: Curve25519PublicKey,

    /// Signatures for the key object.
    signatures: Signatures,

    /// Is the key considered to be a fallback key.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "double_option")]
    fallback: Option<Option<bool>>,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
}

fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

impl SignedKey {
    /// Creates a new `SignedKey` with the given key and signatures.
    pub fn new(key: Curve25519PublicKey) -> Self {
        Self { key, signatures: Signatures::new(), fallback: None, other: BTreeMap::new() }
    }

    /// Creates a new `SignedKey`, that represents a fallback key, with the
    /// given key and signatures.
    pub fn new_fallback(key: Curve25519PublicKey) -> Self {
        Self {
            key,
            signatures: Signatures::new(),
            fallback: Some(Some(true)),
            other: BTreeMap::new(),
        }
    }

    /// Base64-encoded 32-byte Curve25519 public key.
    pub fn key(&self) -> Curve25519PublicKey {
        self.key
    }

    /// Signatures for the key object.
    pub fn signatures(&self) -> &Signatures {
        &self.signatures
    }

    /// Signatures for the key object as a mutable borrow.
    pub fn signatures_mut(&mut self) -> &mut Signatures {
        &mut self.signatures
    }

    /// Is the key considered to be a fallback key.
    pub fn fallback(&self) -> bool {
        self.fallback.map(|f| f.unwrap_or_default()).unwrap_or_default()
    }

    /// Serialize the one-time key into a Raw version.
    pub fn into_raw<T>(self) -> Raw<T> {
        let key = OneTimeKey::SignedKey(self);
        Raw::from_json(to_raw_value(&key).expect("Couldn't serialize one-time key"))
    }
}

/// A one-time public key for "pre-key" messages.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum OneTimeKey {
    /// A signed Curve25519 one-time key.
    SignedKey(SignedKey),
}

impl OneTimeKey {
    /// Deserialize the [`OneTimeKey`] from a [`OneTimeKeyAlgorithm`] and a Raw
    /// JSON value.
    pub fn deserialize(
        algorithm: OneTimeKeyAlgorithm,
        key: &Raw<ruma::encryption::OneTimeKey>,
    ) -> Result<Self, serde_json::Error> {
        match algorithm {
            OneTimeKeyAlgorithm::SignedCurve25519 => {
                let key: SignedKey = key.deserialize_as_unchecked()?;
                Ok(OneTimeKey::SignedKey(key))
            }
            _ => Err(serde::de::Error::custom(format!("Unsupported key algorithm {algorithm}"))),
        }
    }
}

impl OneTimeKey {
    /// Is the key considered to be a fallback key.
    pub fn fallback(&self) -> bool {
        match self {
            OneTimeKey::SignedKey(s) => s.fallback(),
        }
    }
}

#[cfg(test)]
mod tests {
    use ruma::{DeviceKeyAlgorithm, DeviceKeyId, device_id, user_id};
    use serde_json::json;
    use vodozemac::{Curve25519PublicKey, Ed25519Signature};

    use crate::types::{Signature, SignedKey};

    #[test]
    fn serialization() {
        let user_id = user_id!("@user:example.com");
        let device_id = device_id!("EGURVBUNJP");

        let json = json!({
          "key":"XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM",
          "signatures": {
            user_id: {
              "ed25519:EGURVBUNJP": "mia28GKixFzOWKJ0h7Bdrdy2fjxiHCsst1qpe467FbW85H61UlshtKBoAXfTLlVfi0FX+/noJ8B3noQPnY+9Cg",
              "other:EGURVBUNJP": "UnknownSignature"
            }
          },
          "extra_key": "extra_value"
        });

        let curve_key =
            Curve25519PublicKey::from_base64("XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM")
                .expect("Can't construct curve key from base64");

        let signature = Ed25519Signature::from_base64(
            "mia28GKixFzOWKJ0h7Bdrdy2fjxiHCsst1qpe467FbW85H61UlshtKBoAXfTLlVfi0FX+/noJ8B3noQPnY+9Cg"
        ).expect("The signature can always be decoded");

        let custom_signature = Signature::Other("UnknownSignature".to_owned());

        let key: SignedKey =
            serde_json::from_value(json.clone()).expect("Can't deserialize a valid one-time key");

        assert_eq!(key.key(), curve_key);
        assert_eq!(
            key.signatures().get_signature(
                user_id,
                &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, device_id)
            ),
            Some(signature)
        );
        assert_eq!(
            key.signatures()
                .get(user_id)
                .unwrap()
                .get(&DeviceKeyId::from_parts("other".into(), device_id)),
            Some(&Ok(custom_signature))
        );

        let serialized = serde_json::to_value(key).expect("Can't reserialize a signed key");

        assert_eq!(json, serialized);
    }
}
