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
// limitations under the License

use std::sync::Arc;

use rustls::{
    RootCertStore,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, UnixTime, pem::PemObject},
    server::{VerifierBuilderError, WebPkiClientVerifier, danger::ClientCertVerifier},
};
use thiserror::Error;
use vodozemac::base64_decode;
use webpki::EndEntityCert;

use crate::{types::X509Signature, x509::x509_verify::RawX509Verifier};

#[derive(Error, Debug)]
pub enum RustX509VerifyError {
    /// There was an error parsing the certificate
    #[error("failed to parse certificate")]
    ParseError(rustls::pki_types::pem::Error),

    /// There was an error building the verifier
    #[error("failed to build verifier {0}")]
    VerifierBuilderError(VerifierBuilderError),

    /// There was an error adding the certificate to the root store.
    #[error("failed to add certificate to root store {0}")]
    StoreError(rustls::Error),
}

/// A Rust implementation of [`RawX509Verifier`]. This does the verification
/// itself (using `rustls`) rather than delegating the work to some external
/// system.
#[derive(Debug, Clone)]
pub struct RustRawX509Verifier {
    verifier: Arc<dyn ClientCertVerifier>,
}

impl RustRawX509Verifier {
    /// Create a new `RustRawX509Verifier` from the supplied CA certificates
    /// PEM.
    pub fn new_from_pem_data(ca_certs_pem: &str) -> Result<Self, RustX509VerifyError> {
        let mut root_store = RootCertStore::empty();
        for result in CertificateDer::pem_slice_iter(ca_certs_pem.as_bytes()) {
            root_store
                .add(result.map_err(RustX509VerifyError::ParseError)?)
                .map_err(RustX509VerifyError::StoreError)?;
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(root_store))
            //.with_crls(...)
            .build()
            .map_err(RustX509VerifyError::VerifierBuilderError)?;

        Ok(Self { verifier })
    }
}

impl RawX509Verifier for RustRawX509Verifier {
    fn verify(&self, message: &[u8], sig: &X509Signature) -> bool {
        let mut cert_iter = CertificateDer::pem_slice_iter(sig.certificate_chain.as_bytes());
        let Some(Ok(end_cert)) = cert_iter.next() else {
            tracing::warn!("Missing or invalid first certificate");
            return false;
        };
        let Ok(intermediate_certs): Result<Vec<_>, _> = cert_iter.collect() else {
            tracing::warn!("Invalid certificate in the chain");
            return false;
        };

        // Verify the certificate chain is issued by one of our trusted roots
        if self
            .verifier
            .verify_client_cert(&end_cert, intermediate_certs.as_ref(), UnixTime::now())
            .is_err()
        {
            // Not an error: could happen if some certificate has expired.
            return false;
        }

        // Verify this is a signature of the master key
        let Some(provider) = CryptoProvider::get_default() else {
            tracing::error!("Unable to get default rustls crypto provider");
            return false;
        };

        let Some(alg) = provider
            .signature_verification_algorithms
            .mapping
            .iter()
            // Filter for entries with the right signature scheme
            .filter(|item| item.0 == sig.signature_scheme)
            // Filter for entries with a non-empty list of algorithm implementations, and get the
            // first such implementation
            .filter_map(|item| item.1.first().map(|alg| *alg))
            .next()
        else {
            tracing::warn!("Signature scheme {:?} not supported", sig.signature_scheme);
            return false;
        };

        let Ok(cert) = EndEntityCert::try_from(&end_cert) else {
            tracing::warn!("Unable to parse certificate");
            return false;
        };

        // TODO: AJB: make it harder to forget to base64 encode/decode this?
        let Ok(signature_bin) = base64_decode(&sig.signature) else {
            tracing::warn!("Failed to base64-decode the signaturea");
            return false;
        };
        let result = cert.verify_signature(alg, message, &signature_bin);

        if let Err(e) = result {
            tracing::warn!("Signature verification failed: {e}");
            return false;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use crate::x509::{
        RawX509Signer, RawX509Verifier, rust_raw_x509_signer::RustRawX509Signer,
        rust_raw_x509_verifier::RustRawX509Verifier,
        tests::cert_and_key_with_email_in_subject_distinguished_name,
    };

    #[test]
    fn test_can_verify() {
        let (cert, signing_key) =
            cert_and_key_with_email_in_subject_distinguished_name("alice@localhost");

        let cert_pem = cert.pem();
        let key_pem = signing_key.serialize_pem();

        let x509_sign = RustRawX509Signer::new_from_pem_data(&cert_pem, &key_pem).unwrap();

        // After we sign a text
        let (_key_id, sig) = x509_sign.sign(b"hello world").unwrap();

        let x509_verify = RustRawX509Verifier::new_from_pem_data(&cert_pem).unwrap();

        // checking the signature on the same string should succeed
        assert!(x509_verify.verify(b"hello world", &sig));

        // checking the signature on a different string should fail
        assert!(!x509_verify.verify(b"Hello World", &sig));

        // checking the signature with an unknown algorithm should fail
        let sig_with_unknown_alg = {
            let mut sig = sig.clone();
            sig.signature_scheme = 0.into();
            sig
        };
        assert!(!x509_verify.verify(b"hello world", &sig_with_unknown_alg));

        // checking the signature with a bad certificate should fail
        let sig_with_bad_certificate_chain = {
            let mut sig = sig.clone();
            sig.certificate_chain = "".to_owned();
            sig
        };
        assert!(!x509_verify.verify(b"hello world", &sig_with_bad_certificate_chain));
    }
}
