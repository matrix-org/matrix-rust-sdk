use std::sync::Arc;

use ruma::{UserId, canonical_json::to_canonical_value};
use serde_json::json;

use crate::{
    SignatureError,
    olm::utility::to_signable_json,
    types::{CrossSigningKey, X509Signature},
};

#[derive(Debug, Clone)]
pub struct X509Signer {
    x509_sign: Arc<dyn X509Sign>,
}

impl X509Signer {
    pub fn new(x509_sign: Arc<dyn X509Sign>) -> Self {
        Self { x509_sign }
    }

    /// Add a signature to the given cross-signing key using our private X.509
    /// key
    pub fn sign_cross_signing_key(
        &self,
        signing_user_id: &UserId,
        cross_signing_key: &mut CrossSigningKey,
    ) -> Result<(), SignatureError> {
        let json = to_signable_json(to_canonical_value(&cross_signing_key)?)?;

        let (key_id, signature) = self.x509_sign.sign(json.as_bytes())?;

        let device_key_id =
            serde_json::from_value(json!(key_id)).expect("Failed to deserialize device key id");

        cross_signing_key.signatures.add_signature(
            signing_user_id.to_owned(),
            device_key_id,
            signature,
        );

        Ok(())
    }
}

/// A low-level interface for signing messages with a private key. We have Rust
/// and platform-specific implementations.
pub trait X509Sign: std::fmt::Debug + Send + Sync {
    /// Create a signature for the given message using our private key
    ///
    /// Returns (key ID, signature)
    fn sign(&self, message: &[u8]) -> Result<(String, X509Signature), SignatureError>;
}
