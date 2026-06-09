use std::sync::Arc;

use ruma::{OwnedDeviceKeyId, UserId, canonical_json::to_canonical_value};

use crate::{
    SignatureError,
    olm::utility::to_signable_json,
    types::{CrossSigningKey, X509Signature},
};

/// Hold one of these if you want to sign cross-signing keys, and call
/// [`Self::sign_cross_signing_key`] to do it.
///
/// Internally, this holds an implementation of [`RawX509Signer`] that does the
/// real work of signing things. This struct provides a convenient wrapper that
/// e.g. converts a cross-signing key to signable canonical JSON.
#[derive(Debug, Clone)]
pub struct X509Signer {
    x509_sign: Arc<dyn RawX509Signer>,
}

impl X509Signer {
    /// Create a new `X509Signer` that wraps the supplied [`RawX509Signer`].
    pub fn new(x509_sign: Arc<dyn RawX509Signer>) -> Self {
        Self { x509_sign }
    }

    /// Add a signature to the given cross-signing key using our private X.509
    /// key.
    pub fn sign_cross_signing_key(
        &self,
        signing_user_id: &UserId,
        cross_signing_key: &mut CrossSigningKey,
    ) -> Result<(), SignatureError> {
        let json = to_signable_json(to_canonical_value(&cross_signing_key)?)?;

        let (device_key_id, signature) = self.x509_sign.sign(json.as_bytes())?;

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
pub trait RawX509Signer: std::fmt::Debug + Send + Sync {
    /// Create a signature for the given message using our private key
    ///
    /// Returns (key ID, signature)
    fn sign(&self, message: &[u8]) -> Result<(OwnedDeviceKeyId, X509Signature), SignatureError>;
}
