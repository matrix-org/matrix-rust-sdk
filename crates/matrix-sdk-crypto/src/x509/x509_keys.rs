// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::sync::Arc;

use ruma::{UserId, canonical_json::to_canonical_value};
use rustls::{SignatureScheme, sign::SigningKey};
use serde::Serialize;
use serde_json::json;
use vodozemac::base64_encode;

use crate::{
    SignatureError,
    olm::utility::to_signable_json,
    types::{CrossSigningKey, X509Signature},
};

#[derive(Clone)]
pub struct X509Keys {
    signing_key: Arc<dyn SigningKey>,
}

impl X509Keys {
    /// Add a signature to the given cross-signing key using our private key
    pub(crate) fn sign_cross_signing_key(
        &self,
        signing_user_id: &UserId,
        cross_signing_key: &mut CrossSigningKey,
    ) -> Result<(), SignatureError> {
        let signature = self.sign_object(&cross_signing_key)?;

        // TODO RAV: key id
        let device_key_id = serde_json::from_value(json!("x509:todo_key_id"))
            .expect("Failed to deserialize device key id");

        cross_signing_key.signatures.add_signature(
            signing_user_id.to_owned(),
            device_key_id,
            signature,
        );

        Ok(())
    }

    /// Create a signature for the given object using our private key
    fn sign_object<T: Serialize>(&self, object: &T) -> Result<X509Signature, SignatureError> {
        let json = to_signable_json(to_canonical_value(object)?)?;

        // TODO RAV: error handling

        let signature_scheme = SignatureScheme::RSA_PSS_SHA512;
        let signer = self
            .signing_key
            .choose_scheme(&[signature_scheme])
            .expect("unable to choose signature scheme");

        let signature = signer.sign(json.as_bytes()).expect("unable to sign");
        Ok(X509Signature {
            certificate_chain: "TODO".to_owned(),
            signature_scheme,
            signature: base64_encode(signature),
        })
    }
}

impl std::fmt::Debug for X509Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("X509Keys").field(&"<redacted>".to_owned()).finish()
    }
}
