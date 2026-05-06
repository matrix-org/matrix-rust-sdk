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
    server::{WebPkiClientVerifier, danger::ClientCertVerifier},
};
use vodozemac::base64_decode;
use webpki::EndEntityCert;

use crate::{types::X509Signature, x509::x509_verify::X509Verify};

#[derive(Debug, Clone)]
pub struct RustX509Verify {
    verifier: Arc<dyn ClientCertVerifier>,
}

/// A Rust implementation of [`X509Verify`]. This does the verification itself
/// (using `rustls`) rather than delegating the work to some external system.
impl RustX509Verify {
    /// Create a new `RustX509Verify` from the supplied CA certificates PEM.
    pub fn new_from_pem_data(ca_certs_pem: &str) -> Self {
        let mut root_store = RootCertStore::empty();
        for result in CertificateDer::pem_slice_iter(ca_certs_pem.as_bytes()) {
            root_store
                .add(result.expect("Unable to parse certificate in root store"))
                .expect("Unable to add certificate to root store");
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(root_store))
            //.with_crls(...)
            .build()
            .unwrap();

        Self { verifier }
    }
}

impl X509Verify for RustX509Verify {
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
        let provider = CryptoProvider::get_default().expect("unable to get default provider");

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
        let signature_bin =
            base64_decode(&sig.signature).expect("Failed to decode base64 signature");
        let result = cert.verify_signature(alg, message, &signature_bin);

        if let Err(e) = result {
            tracing::warn!("Signature verification failed: {e}");
            return false;
        }

        true
    }
}
