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

use std::convert::TryInto;

use ruma::{serde::CanonicalJsonValue, DeviceKeyAlgorithm, DeviceKeyId, UserId};
use serde_json::Value;
use vodozemac::{olm::Account, Ed25519SecretKey, Ed25519Signature};

use crate::error::SignatureError;

pub trait SignJson {
    fn sign_json(&self, value: Value) -> Result<Ed25519Signature, SignatureError>;

    fn to_signable_json(mut value: Value) -> Result<String, SignatureError> {
        let json_object = value.as_object_mut().ok_or(SignatureError::NotAnObject)?;
        let _ = json_object.remove("signatures");
        let _ = json_object.remove("unsigned");

        let canonical_json: CanonicalJsonValue = value.try_into()?;
        Ok(canonical_json.to_string())
    }
}

impl SignJson for Account {
    fn sign_json(&self, value: Value) -> Result<Ed25519Signature, SignatureError> {
        let serialized = Self::to_signable_json(value)?;

        Ok(self.sign(serialized.as_ref()))
    }
}

impl SignJson for Ed25519SecretKey {
    fn sign_json(&self, value: Value) -> Result<Ed25519Signature, SignatureError> {
        let serialized = Self::to_signable_json(value)?;

        Ok(self.sign(serialized.as_ref()))
    }
}

pub trait VerifyJson {
    /// Verify a signed JSON object.
    ///
    /// The object must have a signatures key associated  with an object of the
    /// form `user_id: {key_id: signature}`.
    ///
    /// Returns Ok if the signature was successfully verified, otherwise an
    /// SignatureError.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user who signed the JSON object.
    ///
    /// * `key_id` - The id of the key that signed the JSON object.
    ///
    /// * `json` - The JSON object that should be verified.
    fn verify_json(
        &self,
        user_id: &UserId,
        key_id: &DeviceKeyId,
        json: &mut Value,
    ) -> Result<(), SignatureError>;
}

impl VerifyJson for vodozemac::Ed25519PublicKey {
    fn verify_json(
        &self,
        user_id: &UserId,
        key_id: &DeviceKeyId,
        json: &mut Value,
    ) -> Result<(), SignatureError> {
        if key_id.algorithm() != DeviceKeyAlgorithm::Ed25519 {
            return Err(SignatureError::UnsupportedAlgorithm);
        }

        let json_object = json.as_object_mut().ok_or(SignatureError::NotAnObject)?;
        let unsigned = json_object.remove("unsigned");
        let signatures = json_object.remove("signatures");

        let canonical_json: CanonicalJsonValue =
            json.clone().try_into().map_err(|_| SignatureError::NotAnObject)?;

        let canonical_json: String = canonical_json.to_string();

        let signatures = signatures.ok_or(SignatureError::NoSignatureFound)?;
        let signature_object = signatures.as_object().ok_or(SignatureError::NoSignatureFound)?;
        let signature =
            signature_object.get(user_id.as_str()).ok_or(SignatureError::NoSignatureFound)?;
        let signature =
            signature.get(key_id.to_string()).ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature.as_str().ok_or(SignatureError::NoSignatureFound)?;

        let signature = vodozemac::Ed25519Signature::from_base64(signature)?;

        let ret = self
            .verify(canonical_json.as_bytes(), &signature)
            .map_err(SignatureError::VerificationError);

        let json_object = json.as_object_mut().ok_or(SignatureError::NotAnObject)?;

        if let Some(u) = unsigned {
            json_object.insert("unsigned".to_owned(), u);
        }

        json_object.insert("signatures".to_owned(), signatures);

        ret
    }
}

#[cfg(test)]
mod tests {
    use ruma::{device_id, user_id, DeviceKeyAlgorithm, DeviceKeyId};
    use serde_json::json;
    use vodozemac::Ed25519PublicKey;

    use super::VerifyJson;

    #[test]
    fn signature_test() {
        let mut device_keys = json!({
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

        signing_key
            .verify_json(
                user_id!("@example:localhost"),
                &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, device_id!("GBEWHQOYGS")),
                &mut device_keys,
            )
            .expect("Can't verify device keys");
    }
}
