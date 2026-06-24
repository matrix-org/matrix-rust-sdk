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

mod raw_x509_signature;
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
    use rcgen::{Certificate, CertificateParams, CustomExtension, KeyPair, PublicKeyData, SanType};
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
        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.custom_extensions.push(subject_key_identifier_extension(&signing_key));
        cert_params.distinguished_name.push(
            rcgen::DnType::CustomDnType(OID_PKCS9_EMAIL_ADDRESS.iter().unwrap().collect()),
            email,
        );

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Create a certificate that contains the supplied email address in its
    /// Subject Alternate Name
    pub(crate) fn cert_and_key_with_email_in_subject_alternate_name(
        email: &str,
    ) -> (Certificate, KeyPair) {
        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.custom_extensions.push(subject_key_identifier_extension(&signing_key));
        cert_params.subject_alt_names.push(SanType::Rfc822Name(
            email.try_into().expect("Failed to convert email address to Ia5String"),
        ));

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Create a certificate that contains the supplied Matrix user ID as a URI
    /// in its Subject Alternate Name
    pub(crate) fn cert_and_key_with_user_id_in_subject_alternate_name(
        user_id: &str,
    ) -> (Certificate, KeyPair) {
        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        let user_id_uri =
            OwnedUserId::try_from(user_id).expect("Invalid user ID!").matrix_uri(false);

        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.custom_extensions.push(subject_key_identifier_extension(&signing_key));
        cert_params.subject_alt_names.push(SanType::URI(
            user_id_uri.to_string().try_into().expect("Failed to convert user_id URI to Ia5String"),
        ));

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Create a certificate that does not contain a user ID or email address
    pub(crate) fn cert_and_key_with_no_user_id() -> (Certificate, KeyPair) {
        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.custom_extensions.push(subject_key_identifier_extension(&signing_key));

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Build an X.509 "SubjectKeyIdentifier" extension for a cert with the
    /// given public key.
    pub(crate) fn subject_key_identifier_extension(
        signing_key: &impl PublicKeyData,
    ) -> CustomExtension {
        // Ref https://www.rfc-editor.org/info/rfc5280/#section-4.2.1.2

        use sha2::{Digest, Sha256};

        // The actual bytes in the SKI don't actually matter that much (and the RFC just
        // makes a couple of suggestions): they just need to be a reasonably
        // unique way of referring to the certificate with the right public key.
        let spki = signing_key.subject_public_key_info();
        let spki_hash = Sha256::digest(&spki);
        let ski_bytes = &spki_hash.as_slice()[0..20];

        // Hacky encoding of the bytes as a DER OCTET-STRING
        let ski_der = [&[0x04, ski_bytes.len() as u8], ski_bytes].concat();

        CustomExtension::from_oid_content(
            &[2, 5, 29, 14], // subjectKeyIdentifier
            ski_der,
        )
    }
}
