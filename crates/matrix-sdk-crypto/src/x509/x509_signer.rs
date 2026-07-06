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

use ruma::{DeviceKeyId, UserId, canonical_json::to_canonical_value};

use crate::{
    SignatureError,
    olm::utility::to_signable_json,
    types::{CrossSigningKey, X509_SIGNATURE_ALGORITHM},
    x509::raw_x509_signature::RawX509Signature,
};

/// Hold one of these if you want to sign cross-signing keys, and call
/// [`Self::sign_cross_signing_key`] to do it.
///
/// Internally, this holds an implementation of [`RawX509Signer`] that does the
/// real work of signing things. This struct provides a convenient wrapper that
/// e.g. converts a cross-signing key to signable canonical JSON.
#[derive(Debug, Clone)]
pub(crate) struct X509Signer {
    x509_sign: Arc<dyn RawX509Signer>,
}

impl X509Signer {
    /// Create a new `X509Signer` that wraps the supplied [`RawX509Signer`].
    pub(crate) fn new(x509_sign: Arc<dyn RawX509Signer>) -> Self {
        Self { x509_sign }
    }

    /// Add a signature to the given cross-signing key using our private X.509
    /// key.
    pub(crate) fn sign_cross_signing_key(
        &self,
        signing_user_id: &UserId,
        cross_signing_key: &mut CrossSigningKey,
    ) -> Result<(), SignatureError> {
        let json = to_signable_json(to_canonical_value(&cross_signing_key)?)?;

        // We use the authority key identifier as a device ID because it yields
        // a unique signature per CA, which is what we want.
        let (authority_key_identifier, signature) = self
            .x509_sign
            .sign(json.as_bytes())?
            .into_x509_signature()
            .map_err(|e| SignatureError::X509SigningError(e.into()))?;

        cross_signing_key.signatures.add_signature(
            signing_user_id.to_owned(),
            DeviceKeyId::from_parts(X509_SIGNATURE_ALGORITHM.into(), &authority_key_identifier),
            signature,
        );

        Ok(())
    }
}

/// A low-level interface for signing messages with a private key. We have Rust
/// and platform-specific implementations.
pub trait RawX509Signer: std::fmt::Debug + Send + Sync {
    /// Create a signature for the given message using our private key
    ///
    /// Returns (key ID, signature)
    fn sign(&self, message: &[u8]) -> Result<RawX509Signature, SignatureError>;
}
