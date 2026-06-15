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

//! Types and traits for verification of users and devices using X.509 keys and
//! certificates.

mod rust_raw_x509_signer;
mod rust_raw_x509_verifier;
mod x509_signer;
mod x509_verify;

pub use rust_raw_x509_signer::RustRawX509Signer;
pub use x509_signer::{RawX509Signer, X509Signer};
pub use x509_verify::{RawX509Verifier, X509Verifier};

#[cfg(test)]
pub(crate) mod tests {
    use rcgen::{Certificate, CertificateParams, KeyPair};
    use x509_parser::oid_registry::OID_PKCS9_EMAIL_ADDRESS;

    /// Create a certificate that contains the supplied email address in its
    /// Subject Distinguished Name
    ///
    /// Note: https://www.rfc-editor.org/rfc/rfc5280.html#section-4.1.2.6 says:
    ///
    /// > Legacy implementations exist where an electronic mail address is
    /// > embedded in the subject distinguished name as an emailAddress
    /// > attribute [RFC2985].
    ///
    /// So this is a legacy implementation. To be non-legacy it should, at a
    /// minimum, include the email address in the Subject Alternative Name
    /// as well as in Subject Distinguished Name.
    pub(crate) fn cert_and_key_with_email(email: &str) -> (Certificate, KeyPair) {
        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.distinguished_name.push(
            rcgen::DnType::CustomDnType(OID_PKCS9_EMAIL_ADDRESS.iter().unwrap().collect()),
            email,
        );

        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        (
            cert_params.self_signed(&signing_key).expect("Failed to generate certificate"),
            signing_key,
        )
    }
}
