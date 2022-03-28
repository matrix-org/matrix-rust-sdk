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

use ruma::{serde::Raw, DeviceKeyId, UserId};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{value::to_raw_value, Value};
use vodozemac::{Curve25519PublicKey, Ed25519Signature};

/// Signatures for a `SignedKey` object.
pub type SignedKeySignatures = BTreeMap<Box<UserId>, BTreeMap<Box<DeviceKeyId>, Ed25519Signature>>;

/// A key for the SignedCurve25519 algorithm
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SignedKey {
    // /// The Curve25519 key that can be used to establish Olm sessions.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    key: Curve25519PublicKey,

    /// Signatures for the key object.
    #[serde(deserialize_with = "deserialize_signatures", serialize_with = "serialize_signatures")]
    signatures: SignedKeySignatures,

    /// Is the key considered to be a fallback key.
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "double_option")]
    fallback: Option<Option<bool>>,

    #[serde(flatten)]
    other: BTreeMap<String, Value>,
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

fn deserialize_signatures<'de, D>(de: D) -> Result<SignedKeySignatures, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let map: BTreeMap<Box<UserId>, BTreeMap<Box<DeviceKeyId>, String>> =
        Deserialize::deserialize(de)?;

    map.into_iter()
        .map(|(u, m)| {
            Ok((
                u,
                m.into_iter()
                    .map(|(d, s)| {
                        Ok((
                            d,
                            Ed25519Signature::from_base64(&s).map_err(serde::de::Error::custom)?,
                        ))
                    })
                    .collect::<Result<BTreeMap<Box<DeviceKeyId>, Ed25519Signature>, _>>()?,
            ))
        })
        .collect::<Result<SignedKeySignatures, _>>()
}

fn serialize_signatures<S>(signatures: &SignedKeySignatures, s: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let signatures: BTreeMap<&Box<UserId>, BTreeMap<&Box<DeviceKeyId>, String>> = signatures
        .iter()
        .map(|(u, m)| (u, m.iter().map(|(d, s)| (d, s.to_base64())).collect()))
        .collect();

    signatures.serialize(s)
}

fn double_option<'de, T, D>(de: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: serde::Deserializer<'de>,
{
    Deserialize::deserialize(de).map(Some)
}

impl SignedKey {
    /// Creates a new `SignedKey` with the given key and signatures.
    pub fn new(key: Curve25519PublicKey) -> Self {
        Self { key, signatures: BTreeMap::new(), fallback: None, other: BTreeMap::new() }
    }

    /// Creates a new `SignedKey`, that represents a fallback key, with the
    /// given key and signatures.
    pub fn new_fallback(key: Curve25519PublicKey) -> Self {
        Self {
            key,
            signatures: BTreeMap::new(),
            fallback: Some(Some(true)),
            other: BTreeMap::new(),
        }
    }

    /// Base64-encoded 32-byte Curve25519 public key.
    pub fn key(&self) -> Curve25519PublicKey {
        self.key
    }

    /// Signatures for the key object.
    pub fn signatures(&mut self) -> &mut SignedKeySignatures {
        &mut self.signatures
    }

    /// Is the key considered to be a fallback key.
    pub fn fallback(&self) -> bool {
        self.fallback.map(|f| f.unwrap_or_default()).unwrap_or_default()
    }

    /// Serialize the one-time key into a Raw version.
    pub fn into_raw<T>(self) -> Raw<T> {
        let key = OneTimeKey::SignedKey(self);
        Raw::from_json(to_raw_value(&key).expect("Coulnd't serialize one-time key"))
    }
}

/// A one-time public key for "pre-key" messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum OneTimeKey {
    /// A signed Curve25519 one-time key.
    SignedKey(SignedKey),

    /// An unsigned Curve25519 one-time key.
    #[serde(deserialize_with = "deserialize_curve_key", serialize_with = "serialize_curve_key")]
    Key(Curve25519PublicKey),

    /// An unknown one-time key type.
    Other(Value),
}

#[cfg(test)]
mod test {
    use matches::assert_matches;
    use serde_json::json;
    use vodozemac::Curve25519PublicKey;

    use super::OneTimeKey;

    #[test]
    fn serialization() {
        let json = json!({
          "key":"XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM",
          "signatures": {
            "@user:example.com": {
              "ed25519:EGURVBUNJP": "mia28GKixFzOWKJ0h7Bdrdy2fjxiHCsst1qpe467FbW85H61UlshtKBoAXfTLlVfi0FX+/noJ8B3noQPnY+9Cg"
            }
          },
          "extra_key": "extra_value"
        });

        let curve_key =
            Curve25519PublicKey::from_base64("XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM")
                .expect("Can't construct curve key from base64");

        let key: OneTimeKey =
            serde_json::from_value(json.clone()).expect("Can't deserialize a valid one-time key");

        assert_matches!(key, OneTimeKey::SignedKey(ref k) if k.key == curve_key);

        let serialized = serde_json::to_value(key).expect("Can't reserialize a signed key");

        assert_eq!(json, serialized);
    }
}
