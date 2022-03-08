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

use olm_rs::utility::OlmUtility;
use ruma::{serde::CanonicalJsonValue, DeviceKeyAlgorithm, DeviceKeyId, UserId};
use serde_json::Value;

use crate::error::SignatureError;

pub(crate) struct Utility {
    inner: OlmUtility,
}

impl Utility {
    pub fn new() -> Self {
        Self { inner: OlmUtility::new() }
    }

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
    /// * `signing_key` - The public ed25519 key which was used to sign the JSON
    ///   object.
    ///
    /// * `json` - The JSON object that should be verified.
    pub(crate) fn verify_json(
        &self,
        user_id: &UserId,
        key_id: &DeviceKeyId,
        signing_key: &str,
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

        let ret =
            match self.inner.ed25519_verify(signing_key, &canonical_json, signature.to_owned()) {
                Ok(_) => Ok(()),
                Err(_) => Err(SignatureError::VerificationError),
            };

        let json_object = json.as_object_mut().ok_or(SignatureError::NotAnObject)?;

        if let Some(u) = unsigned {
            json_object.insert("unsigned".to_owned(), u);
        }

        json_object.insert("signatures".to_owned(), signatures);

        ret
    }
}

#[cfg(test)]
mod test {
    use ruma::{device_id, user_id, DeviceKeyAlgorithm, DeviceKeyId};
    use serde_json::json;

    use super::Utility;

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

        let utility = Utility::new();

        utility
            .verify_json(
                user_id!("@example:localhost"),
                &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, device_id!("GBEWHQOYGS")),
                signing_key,
                &mut device_keys,
            )
            .expect("Can't verify device keys");
    }
}
