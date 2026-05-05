use std::{fmt::Debug, sync::Arc};

use ruma::UserId;

use crate::types::{Signature, X509Signature};

/// Hold one of these if you want to verify X.509 signatures, and call
/// [`Self::verify_x509_signature`] to do it.
///
/// Internally, this holds an implementation of [`X509Verify`] that does the
/// real work of verifying things. This struct provides a convenient wrapper.
#[derive(Debug, Clone)]
pub struct X509Verifier {
    x509_verify: Arc<dyn X509Verify>,
}

impl X509Verifier {
    /// Create a new `X509Verifier` that wraps the supplied [`X509Verify`].
    pub fn new(x509_verify: Arc<dyn X509Verify>) -> X509Verifier {
        X509Verifier { x509_verify }
    }

    /// Check if the given signature is a valid X.509 signature for the given
    /// message.
    ///
    /// Also validates that the certificate used for the signature is issued via
    /// one of our trusted CAs, and was issued to the given user id.
    pub fn verify_x509_signature(&self, user_id: &UserId, message: &str, sig: &Signature) -> bool {
        let Signature::X509(sig) = sig else {
            // Not an error: just the wrong type of signature.
            return false;
        };

        self.x509_verify.verify(user_id, message.as_bytes(), sig)
    }
}

/// Something that can verify an X.509 signature.
pub trait X509Verify: Debug + Send + Sync {
    /// Check if the given signature is a valid X.509 signature for the given
    /// message.
    ///
    /// Also validates that the certificate used for the signature is issued via
    /// one of our trusted CAs, and was issued to the given user id.
    fn verify(&self, user_id: &UserId, message: &[u8], sig: &X509Signature) -> bool;
}
