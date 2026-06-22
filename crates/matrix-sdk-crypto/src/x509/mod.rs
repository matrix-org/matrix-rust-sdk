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

pub(crate) mod rust_raw_x509_signer;
pub(crate) mod rust_raw_x509_verifier;
mod x509_signer;
mod x509_verify;

pub use rust_raw_x509_signer::RustRawX509Signer;
pub use rust_raw_x509_verifier::RustRawX509Verifier;
pub use x509_signer::{RawX509Signer, X509Signer};
pub use x509_verify::{RawX509Verifier, X509Verifier};

#[cfg(test)]
pub(crate) mod tests {
    use rcgen::{Certificate, CertificateParams, KeyPair, SanType};
    use ruma::OwnedUserId;
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
    pub(crate) fn cert_and_key_with_email_in_subject_distinguished_name(
        email: &str,
    ) -> (Certificate, KeyPair) {
        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.distinguished_name.push(
            rcgen::DnType::CustomDnType(OID_PKCS9_EMAIL_ADDRESS.iter().unwrap().collect()),
            email,
        );

        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Create a certificate that contains the supplied email address in its
    /// Subject Alternate Name
    pub(crate) fn cert_and_key_with_email_in_subject_alternate_name(
        email: &str,
    ) -> (Certificate, KeyPair) {
        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.subject_alt_names.push(SanType::Rfc822Name(
            email.try_into().expect("Failed to convert email address to Ia5String"),
        ));

        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Create a certificate that contains the supplied Matrix user ID as a URI
    /// in its Subject Alternate Name
    pub(crate) fn cert_and_key_with_user_id_in_subject_alternate_name(
        user_id: &str,
    ) -> (Certificate, KeyPair) {
        let user_id_uri =
            OwnedUserId::try_from(user_id).expect("Invalid user ID!").matrix_uri(false);

        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.subject_alt_names.push(SanType::URI(
            user_id_uri.to_string().try_into().expect("Failed to convert user_id URI to Ia5String"),
        ));

        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Create a certificate that does not contain a user ID or email address
    pub(crate) fn cert_and_key_with_no_user_id() -> (Certificate, KeyPair) {
        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;

        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }
}
