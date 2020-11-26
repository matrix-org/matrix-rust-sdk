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

use olm_rs::utility::OlmUtility;
use serde_json::Value;

use matrix_sdk_common::identifiers::{DeviceKeyAlgorithm, DeviceKeyId, UserId};

use crate::error::SignatureError;

pub(crate) struct Utility {
    inner: OlmUtility,
}

impl Utility {
    pub fn new() -> Self {
        Self {
            inner: OlmUtility::new(),
        }
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
    ///     object.
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

        let canonical_json = serde_json::to_string(json_object)?;

        if let Some(u) = unsigned {
            json_object.insert("unsigned".to_string(), u);
        }

        let signatures = signatures.ok_or(SignatureError::NoSignatureFound)?;
        let signature_object = signatures
            .as_object()
            .ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature_object
            .get(user_id.as_str())
            .ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature
            .get(key_id.to_string())
            .ok_or(SignatureError::NoSignatureFound)?;
        let signature = signature.as_str().ok_or(SignatureError::NoSignatureFound)?;

        let ret = if self
            .inner
            .ed25519_verify(signing_key, &canonical_json, signature)
            .is_ok()
        {
            Ok(())
        } else {
            Err(SignatureError::VerificationError)
        };

        json_object.insert("signatures".to_string(), signatures);

        ret
    }
}
