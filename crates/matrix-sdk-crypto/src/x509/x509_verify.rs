use std::{fmt::Debug, sync::Arc};

use ruma::UserId;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tracing::info;

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
            info!("X509: verify_x509_signature(): not an x509 sig");
            return false;
        };

        // Before we pass over to the X.509 certificate verifier, check that the leaf
        // certificate is valid for the given user_id.
        let mut cert_iter = CertificateDer::pem_slice_iter(sig.certificate_chain.as_bytes());
        let Some(Ok(end_cert)) = cert_iter.next() else {
            tracing::warn!("Missing or invalid first certificate");
            return false;
        };

        let Some(certificate_email) = get_email_address_from_certificate_subject(&end_cert) else {
            tracing::warn!("Certificate subject does not contain an email address");
            return false;
        };

        if certificate_email != map_user_id_to_email(user_id) {
            tracing::warn!("Certificate not valid for this user");
            return false;
        }

        self.x509_verify.verify(message.as_bytes(), sig)
    }
}

/// Something that can verify an X.509 signature.
pub trait X509Verify: Debug + Send + Sync {
    /// Check if the given signature is a valid X.509 signature for the given
    /// message.
    ///
    /// Also validates that the certificate used for the signature is issued via
    /// one of our trusted CAs.
    fn verify(&self, message: &[u8], sig: &X509Signature) -> bool;
}

fn map_user_id_to_email(user_id: &UserId) -> String {
    // TODO RAV: this is not a reliable way to map from user_ids to email addresses.
    format!("{}@{}", user_id.localpart(), user_id.server_name())
}

fn get_email_address_from_certificate_subject(certificate: &CertificateDer<'_>) -> Option<String> {
    use x509_parser::prelude::*;
    let (_, parsed_cert) = X509Certificate::from_der(certificate.as_ref()).ok()?;
    let email = parsed_cert.subject.iter_email().next()?;
    email.as_str().ok().map(ToOwned::to_owned)
}
