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

use ruma::UserId;
use rustls::{
    RootCertStore,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, UnixTime, pem::PemObject},
    server::{WebPkiClientVerifier, danger::ClientCertVerifier},
};
use webpki::EndEntityCert;

use crate::types::Signature;

#[derive(Debug, Clone)]
pub struct X509TrustRoot {
    verifier: Arc<dyn ClientCertVerifier>,
}

impl X509TrustRoot {
    pub fn new(verifier: Arc<dyn ClientCertVerifier>) -> Self {
        Self { verifier }
    }

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

        // Check that the certificate is valid for the given user_id
        let Some(certificate_email) = get_email_address_from_certificate_subject(&end_cert) else {
            tracing::warn!("Certificate subject does not contain an email address");
            return false;
        };

        if certificate_email != map_user_id_to_email(user_id) {
            tracing::warn!("Certificate not valid for this user");
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

        if cert.verify_signature(alg, message.as_bytes(), sig.signature.as_bytes()).is_err() {
            tracing::warn!("Signature verification failed");
            return false;
        }

        true
    }
}

fn map_user_id_to_email(user_id: &UserId) -> String {
    // TODO RAV: this is not a reliable way to map from user_ids to email addresses.
    format!("{}@{}", user_id.localpart(), user_id.server_name())
}

fn get_email_address_from_certificate_subject(certificate: &CertificateDer) -> Option<String> {
    use x509_parser::prelude::*;
    let (_, parsed_cert) = X509Certificate::from_der(certificate.as_ref()).ok()?;
    let email = parsed_cert.subject.iter_email().next()?;
    email.as_str().ok().map(ToOwned::to_owned)
}
