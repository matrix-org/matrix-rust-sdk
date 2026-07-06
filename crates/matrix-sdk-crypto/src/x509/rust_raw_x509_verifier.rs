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
    RootCertStore, SignatureScheme,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, UnixTime, pem::PemObject},
    server::{VerifierBuilderError, WebPkiClientVerifier, danger::ClientCertVerifier},
};
use thiserror::Error;
use webpki::EndEntityCert;

use crate::x509::{
    errors::X509SignatureVerificationError,
    raw_x509_signature::{RawX509Signature, X509SignatureScheme},
    x509_verify::RawX509Verifier,
};

/// An enum of possible errors that can occur while instantiating
/// a [`RustRawX509Verifier`].
#[derive(Error, Debug)]
pub enum RustX509VerifyError {
    /// There was an error parsing the certificate
    #[error("failed to parse certificate")]
    CertificateParse(rustls::pki_types::pem::Error),

    /// There was an error building the verifier
    #[error("failed to build verifier {0}")]
    VerifierBuilder(VerifierBuilderError),

    /// There was an error adding the certificate to the root store.
    #[error("failed to add certificate to root store {0}")]
    Store(rustls::Error),
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
                .add(result.map_err(RustX509VerifyError::CertificateParse)?)
                .map_err(RustX509VerifyError::Store)?;
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(root_store))
            .build()
            .map_err(RustX509VerifyError::VerifierBuilder)?;

        Ok(Self { verifier })
    }
}

impl RawX509Verifier for RustRawX509Verifier {
    fn verify(
        &self,
        message: &[u8],
        sig: &RawX509Signature,
    ) -> Result<(), X509SignatureVerificationError> {
        let mut cert_iter = CertificateDer::pem_slice_iter(sig.certificate_chain.as_bytes());
        let Some(Ok(leaf_cert)) = cert_iter.next() else {
            tracing::warn!("Missing or invalid leaf certificate");
            return Err(X509SignatureVerificationError::MissingOrInvalidFirstCertificate);
        };
        let Ok(intermediate_certs): Result<Vec<_>, _> = cert_iter.collect() else {
            tracing::warn!("Invalid certificate in the chain");
            return Err(X509SignatureVerificationError::InvalidCertificateInChain);
        };

        // Verify the certificate chain is issued by one of our trusted roots
        if self
            .verifier
            .verify_client_cert(&leaf_cert, intermediate_certs.as_ref(), UnixTime::now())
            .is_err()
        {
            // Not an error: could happen if some certificate has expired.
            return Err(X509SignatureVerificationError::CertificateExpired);
        }

        // Verify this is a signature of the master key
        let Some(provider) = CryptoProvider::get_default() else {
            tracing::error!("Unable to get default rustls crypto provider");
            return Err(X509SignatureVerificationError::MissingCryptoProvider);
        };

        let rustls_signature_scheme = match sig.signature_scheme {
            X509SignatureScheme::RsaPssSha512 => SignatureScheme::RSA_PSS_SHA512,
        };

        let Some(alg) = provider
            .signature_verification_algorithms
            .mapping
            .iter()
            // Filter for entries with the right signature scheme
            .filter(|item| item.0 == rustls_signature_scheme)
            // Filter for entries with a non-empty list of algorithm implementations, and get the
            // first such implementation
            .filter_map(|item| item.1.first().copied())
            .next()
        else {
            tracing::warn!("Signature scheme {:?} not supported", sig.signature_scheme);
            return Err(X509SignatureVerificationError::UnsupportedSignatureScheme(
                sig.signature_scheme.clone(),
            ));
        };

        let cert = EndEntityCert::try_from(&leaf_cert)
            .inspect_err(|_| tracing::warn!("Unable to parse certificate"))
            .map_err(|e| X509SignatureVerificationError::Custom(e.into()))?;

        cert.verify_signature(alg, message, sig.signature_bytes.as_slice())
            .inspect_err(|e| tracing::warn!("Signature verification failed: {e}"))
            .map_err(|e| X509SignatureVerificationError::Custom(e.into()))
    }
}
