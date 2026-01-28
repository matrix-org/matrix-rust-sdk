// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use ruma::{
    CanonicalJsonValue, DeviceKeyAlgorithm, DeviceKeyId, UserId, canonical_json::to_canonical_value,
};
use serde::Serialize;
use vodozemac::{Ed25519PublicKey, Ed25519SecretKey, Ed25519Signature, olm::Account};

use crate::{
    error::SignatureError,
    types::{CrossSigningKey, DeviceKeys, Signature, Signatures, SignedKey},
};

fn to_signable_json(mut value: CanonicalJsonValue) -> Result<String, SignatureError> {
    let json_object = value.as_object_mut().ok_or(SignatureError::NotAnObject)?;
    let _ = json_object.remove("signatures");
    let _ = json_object.remove("unsigned");

    Ok(value.to_string())
}

pub trait SignJson {
    fn sign_json(&self, value: CanonicalJsonValue) -> Result<Ed25519Signature, SignatureError>;
}

impl SignJson for Account {
    fn sign_json(&self, value: CanonicalJsonValue) -> Result<Ed25519Signature, SignatureError> {
        let serialized = to_signable_json(value)?;

        Ok(self.sign(serialized))
    }
}

impl SignJson for Ed25519SecretKey {
    fn sign_json(&self, value: CanonicalJsonValue) -> Result<Ed25519Signature, SignatureError> {
        let serialized = to_signable_json(value)?;

        Ok(self.sign(serialized.as_ref()))
    }
}

/// A trait for objects that are able to verify signatures.
pub trait VerifyJson {
    /// Verify a signature over the SignedJsonObject using this public Ed25519
    /// key.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user that claims to have signed this object.
    ///
    /// * `key_id` - The ID of the key that was used to sign this object.
    ///
    ///   **Note**: The key ID must match the ID of the public key that is
    ///   verifying the signature. This is only used to find the correct
    ///   signature.
    ///
    /// * `signed_object` - The signed object that we should check for a valid
    ///   signature.
    ///
    /// Returns Ok if the signature was successfully verified, otherwise an
    /// SignatureError.
    fn verify_json(
        &self,
        user_id: &UserId,
        key_id: &DeviceKeyId,
        signed_object: &impl SignedJsonObject,
    ) -> Result<(), SignatureError>;

    /// Verify a signature over the canonicalized signed JSON object using
    /// this public Ed25519 key.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user that claims to have signed this object.
    ///
    /// * `key_id` - The ID of the key that was used to sign this object.
    ///
    ///   **Note**: The key ID must match the ID of the public key that is
    ///   verifying the signature. This is only used to find the correct
    ///   signature.
    ///
    /// * `canonicalized_json` - The canonicalized version of a signed JSON
    ///   object.
    ///
    /// This method should only be used if an object's signature needs to be
    /// checked multiple times, and you'd like to avoid performing the
    /// canonicalization step each time. Otherwise, prefer the
    /// [`VerifyJson::verify_json`] method
    ///
    /// Returns Ok if the signature was successfully verified, otherwise an
    /// SignatureError.
    fn verify_canonicalized_json(
        &self,
        user_id: &UserId,
        key_id: &DeviceKeyId,
        signatures: &Signatures,
        canonical_json: &str,
    ) -> Result<(), SignatureError>;
}

impl VerifyJson for Ed25519PublicKey {
    fn verify_json(
        &self,
        user_id: &UserId,
        key_id: &DeviceKeyId,
        signed_object: &impl SignedJsonObject,
    ) -> Result<(), SignatureError> {
        if key_id.algorithm() != DeviceKeyAlgorithm::Ed25519 {
            return Err(SignatureError::UnsupportedAlgorithm);
        }

        let canonicalized = signed_object.to_canonical_json()?;
        verify_signature(*self, user_id, key_id, signed_object.signatures(), &canonicalized)
    }

    fn verify_canonicalized_json(
        &self,
        user_id: &UserId,
        key_id: &DeviceKeyId,
        signatures: &Signatures,
        canonical_json: &str,
    ) -> Result<(), SignatureError> {
        if key_id.algorithm() != DeviceKeyAlgorithm::Ed25519 {
            return Err(SignatureError::UnsupportedAlgorithm);
        }

        verify_signature(*self, user_id, key_id, signatures, canonical_json)
    }
}

fn verify_signature(
    public_key: Ed25519PublicKey,
    user_id: &UserId,
    key_id: &DeviceKeyId,
    signatures: &Signatures,
    canonical_json: &str,
) -> Result<(), SignatureError> {
    let s = signatures
        .get(user_id)
        .and_then(|m| m.get(key_id))
        .ok_or(SignatureError::NoSignatureFound)?;

    match s {
        Ok(Signature::Ed25519(s)) => Ok(public_key.verify(canonical_json.as_bytes(), s)?),
        Ok(Signature::Other(_)) => Err(SignatureError::UnsupportedAlgorithm),
        Err(_) => Err(SignatureError::InvalidSignature),
    }
}

/// A trait for Matrix objects that we can canonicalize, sign and verify
/// signatures for, as described by the [spec].
///
/// [spec]: https://spec.matrix.org/unstable/appendices/#signing-json
pub trait SignedJsonObject: Serialize {
    /// Get the collection of signatures present on this signed JSON object.
    fn signatures(&self) -> &Signatures;

    /// Convert this signed JSON object to a canonicalized signed JSON string.
    fn to_canonical_json(&self) -> Result<String, SignatureError> {
        let value = to_canonical_value(self)?;
        to_signable_json(value)
    }
}

impl SignedJsonObject for DeviceKeys {
    fn signatures(&self) -> &Signatures {
        &self.signatures
    }
}

impl SignedJsonObject for SignedKey {
    fn signatures(&self) -> &Signatures {
        self.signatures()
    }
}

impl SignedJsonObject for CrossSigningKey {
    fn signatures(&self) -> &Signatures {
        &self.signatures
    }
}

impl SignedJsonObject for crate::types::MegolmV1AuthData {
    fn signatures(&self) -> &Signatures {
        &self.signatures
    }
}

#[cfg(test)]
mod tests {
    use ruma::{DeviceKeyAlgorithm, DeviceKeyId, device_id, user_id};
    use serde_json::json;
    use vodozemac::Ed25519PublicKey;

    use super::VerifyJson;
    use crate::types::DeviceKeys;

    #[test]
    fn signature_test() {
        let device_keys = json!({
            "device_id": "GBEWHQOYGS",
            "algorithms": [
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
            ],
            "keys": {
                "curve25519:GBEWHQOYGS": "F8QhZ0Z1rjtWrQOblMDgZtEX5x1UrG7sZ2Kk3xliNAU",
                "ed25519:GBEWHQOYGS": "n469gw7zm+KW+JsFIJKnFVvCKU14HwQyocggcCIQgZY"
            },
            "signatures": {
                "@example:localhost": {
                    "ed25519:GBEWHQOYGS": "OlF2REsqjYdAfr04ONx8VS/5cB7KjrWYRlLF4eUm2foAiQL/RAfsjsa2JXZeoOHh6vEualZHbWlod49OewVqBg"
                }
            },
            "unsigned": {
                "device_display_name": "Weechat-Matrix-rs"
            },
            "user_id": "@example:localhost"
        });

        let signing_key = "n469gw7zm+KW+JsFIJKnFVvCKU14HwQyocggcCIQgZY";

        let signing_key = Ed25519PublicKey::from_base64(signing_key)
            .expect("The signing key wasn't proper base64");

        let device_keys: DeviceKeys = serde_json::from_value(device_keys).unwrap();

        signing_key
            .verify_json(
                user_id!("@example:localhost"),
                &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, device_id!("GBEWHQOYGS")),
                &device_keys,
            )
            .expect("Can't verify device keys");
    }
}
