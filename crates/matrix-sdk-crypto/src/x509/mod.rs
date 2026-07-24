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

#![cfg(feature = "experimental-x509-identity-verification")]

//! Types and traits for verification of users and devices using X.509 keys and
//! certificates.
//!
//! This module implements the `io.element.x509` [signing algorithm], which
//! allows a Matrix user's cross-signing identity to be signed with an X.509
//! private key and verified against a set of trusted Certificate Authorities.
//! This provides stronger identity guarantees than Ed25519 cross-signing
//! alone: it lets a user be recognised as a known party, protects against
//! man-in-the-middle attacks without out-of-band interactive verification, and
//! allows identity resets to be accepted without warnings when re-signed by a
//! trusted CA.
//!
//! # Preparing a certificate
//!
//! The user's Matrix ID is bound into the certificate via the Subject
//! Alternative Name (SAN) field, encoded as a `matrix:u/...` URI in a
//! `uniformResourceIdentifier`. As a transitional measure, an `rfc822Name` of
//! the form `localpart@domain` (in the SAN or Subject field) MAY also be
//! accepted for a Matrix ID `@localpart:domain`.
//!
//! # The `io.element.x509` signing algorithm
//!
//! The key identifier is the (usually 20-byte) Subject Key Identifier of the CA
//! that issued the highest-level certificate in the presented chain, encoded as
//! unpadded Base64.
//!
//! As with Ed25519 Matrix signatures, the JSON object to be signed is stripped
//! of any `signatures` and `unsigned` properties and encoded as Canonical JSON,
//! then signed with the private key corresponding to the X.509 certificate.
//! Only RSASSA-PSS signatures using SHA-512 are supported.
//!
//! The signature, leaf certificate, and any intermediate certificates are
//! formatted as a CMS `SignedData` structure ([RFC 5652 §5](https://www.rfc-editor.org/info/rfc5652/#section-5)),
//! wrapped in a `ContentInfo` ([RFC 5652 §3](https://www.rfc-editor.org/info/rfc5652#section-3))
//! and PEM-encoded ([RFC 7468 §9](https://www.rfc-editor.org/info/rfc7468#section-9)).
//! The following constraints apply to the `SignedData` structure for
//! compatibility with other Matrix clients:
//!
//! - `version` MUST be 3.
//! - `digestAlgorithms` MUST contain exactly one entry matching the
//!   `SignerInfo` `digestAlgorithm` (i.e. `sha512`, with NULL or absent
//!   parameters).
//! - `encapContentInfo` MUST NOT contain `eContent`, and `eContentType` MUST be
//!   `id-data` (`1.2.840.113549.1.7.1`).
//! - `certificates` MUST be present, and each entry MUST be a regular X.509
//!   certificate.
//! - `crls` MUST NOT be present.
//! - `signerInfos` MUST contain exactly one entry, with:
//!   - `version` equal to 3,
//!   - `sid` given as the `subjectKeyIdentifier` alternative, referencing the
//!     leaf certificate,
//!   - `digestAlgorithm` of `sha512` (`2.16.840.1.101.3.4.2.3`) with absent or
//!     NULL parameters,
//!   - `signedAttrs` absent, and
//!   - `signatureAlgorithm` representing RSASSA-PSS with SHA-512, MGF1 over
//!     SHA-512, and a salt length of 64:
//!
//! ```text
//! signatureAlgorithm:
//!   algorithm: rsassaPss (1.2.840.113549.1.1.10)
//!   parameters:
//!     hashAlgorithm:
//!       algorithm: sha512 (2.16.840.1.101.3.4.2.3)
//!       parameters: <ABSENT>
//!     maskGenAlgorithm:
//!       algorithm: mgf1 (1.2.840.113549.1.1.8)
//!       parameters:
//!         hashAlgorithm:
//!           algorithm: sha512 (2.16.840.1.101.3.4.2.3)
//!           parameters: <ABSENT>
//!     saltLength: 64
//! ```
//!
//! The `parameters` of the `sha512` `hashAlgorithm` fields MAY be NULL instead
//! of absent (see [RFC 8017 §A.2.3](https://www.rfc-editor.org/info/rfc8017#appendix-A.2.3)).
//!
//! # Verifying an identity
//!
//! A verifier maintains a list of Certificate Authorities it trusts to sign
//! Matrix identities. An identity is considered verified if any
//! `io.element.x509` signature on the public master cross-signing key satisfies
//! all of the following:
//!
//! 1. The signature is a correct signature of the public master cross-signing
//!    key under the public key in the presented leaf certificate.
//! 2. The Subject or Subject Alternative Name fields of the leaf certificate
//!    show that it was issued to the user presenting it.
//! 3. There is a valid certification path from a trusted CA to the leaf
//!    certificate (possibly via presented intermediate certificates), with all
//!    certificates within their validity periods and absent from any revocation
//!    lists the verifier chooses to inspect.
//!
//! [signing algorithm]: https://spec.matrix.org/v1.18/appendices/#signing-details

mod errors;
mod raw_x509_signature;
mod x509_signer;
mod x509_verify;

pub use errors::{
    IntoX509SignatureError, X509SignatureSigningError, X509SignatureVerificationError,
};
pub use raw_x509_signature::RawX509Signature;
pub use x509_signer::RawX509Signer;
pub(crate) use x509_signer::X509Signer;
pub use x509_verify::RawX509Verifier;
pub(crate) use x509_verify::X509Verifier;

#[cfg(any(test, feature = "rust-x509-verifier-impl"))]
mod rust_raw_x509_signer;
#[cfg(any(test, feature = "rust-x509-verifier-impl"))]
mod rust_raw_x509_verifier;
#[cfg(any(test, feature = "rust-x509-verifier-impl"))]
pub use rust_raw_x509_signer::RustRawX509Signer;
#[cfg(any(test, feature = "rust-x509-verifier-impl"))]
pub use rust_raw_x509_verifier::RustRawX509Verifier;

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use rcgen::{
        Certificate, CertificateParams, CustomExtension, DnType, Issuer, KeyPair, PublicKeyData,
        SanType,
    };
    use ruma::OwnedUserId;

    use crate::x509::{RustRawX509Signer, RustRawX509Verifier, X509Signer, X509Verifier};

    pub(crate) const OID_PKCS9_EMAIL_ADDRESS: &[u64] = &[1, 2, 840, 113549, 1, 9, 1];
    pub(crate) const OID_SUBJECT_KEY_IDENTIFIER: &[u64] = &[2, 5, 29, 14];

    /// Generate an RSA key pair suitable for the `io.element.x509` signing
    /// algorithm (RSASSA-PSS with SHA-512).
    pub(crate) fn generate_rsa_key() -> KeyPair {
        KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair")
    }

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
        set_up_default_crypto_provider();

        let signing_key = generate_rsa_key();

        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.custom_extensions.push(subject_key_identifier_extension(&signing_key));
        cert_params.distinguished_name.push(DnType::from_oid(OID_PKCS9_EMAIL_ADDRESS), email);

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Create a certificate that contains the supplied email address in its
    /// Subject Alternate Name
    pub(crate) fn cert_and_key_with_email_in_subject_alternate_name(
        email: &str,
    ) -> (Certificate, KeyPair) {
        set_up_default_crypto_provider();

        let signing_key = generate_rsa_key();

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
        set_up_default_crypto_provider();

        let signing_key = generate_rsa_key();

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
        set_up_default_crypto_provider();

        let signing_key = generate_rsa_key();

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

        CustomExtension::from_oid_content(OID_SUBJECT_KEY_IDENTIFIER, ski_der)
    }

    /// Create a [`CertificateParams`] (for creating a certificate) where the
    /// distinguished name has the CommonName provided.
    pub(crate) fn cert_params(common_name: &str) -> CertificateParams {
        let mut cert_params = CertificateParams::default();
        cert_params.distinguished_name.remove(DnType::CommonName);
        cert_params.distinguished_name.push(DnType::CommonName, common_name);
        cert_params.use_authority_key_identifier_extension = true;
        cert_params
    }

    /// Generate a little certificate authority i.e. a key pair and a
    /// self-signed certificate.
    pub(crate) fn ca_cert() -> (Certificate, KeyPair) {
        set_up_default_crypto_provider();

        let cert_params = cert_params("You Can Trust Us Certificate Authority");

        let signing_key = generate_rsa_key();

        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Generate a key pair and a certificate, containing the supplied email
    /// address in its Subject Distinguished Name, signed by the supplied
    /// certificate authority.
    pub(crate) fn cert_and_key_with_email_signed_by(
        email: &str,
        ca_cert: &Certificate,
        ca_signing_key: &KeyPair,
    ) -> (Certificate, KeyPair) {
        let signing_key = generate_rsa_key();

        let mut cert_params = cert_params(&format!("Cert for {email}"));
        cert_params.distinguished_name.push(DnType::from_oid(OID_PKCS9_EMAIL_ADDRESS), email);
        cert_params.custom_extensions.push(subject_key_identifier_extension(&signing_key));

        let issuer = Issuer::from_ca_cert_pem(&ca_cert.pem(), ca_signing_key)
            .expect("Failed to create Issuer from CA cert and key");

        let cert =
            cert_params.signed_by(&signing_key, &issuer).expect("Failed to generate certificate");

        (cert, signing_key)
    }

    /// Create a [`X509Signer`] and [`X509Verifier`] pair from a certificate and
    /// signing key. This uses the testing-only Rust implementations.
    pub(crate) fn create_rust_signer_and_verifier(
        cert: Certificate,
        signing_key: KeyPair,
    ) -> (X509Signer, X509Verifier) {
        let cert_pem = cert.pem();
        let key_pem = signing_key.serialize_pem();

        let x509_signer = {
            let rust_raw_x509_signer =
                RustRawX509Signer::new_from_pem_data(&cert_pem, &key_pem).unwrap();
            X509Signer::new(Arc::new(rust_raw_x509_signer))
        };

        let x509_verifier = {
            let rust_raw_x509_verifier = RustRawX509Verifier::new_from_pem_data(&cert_pem).unwrap();
            X509Verifier::new(Arc::new(rust_raw_x509_verifier))
        };
        (x509_signer, x509_verifier)
    }

    fn set_up_default_crypto_provider() {
        // Ignore errors: they just mean that this method was called before.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    // Create three X.509 signers with different validity periods.
    pub(crate) fn signers_with_different_validity() -> (X509Signer, X509Signer, X509Signer) {
        let signing_key =
            KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

        let mut cert_params = CertificateParams::default();
        cert_params.use_authority_key_identifier_extension = true;
        cert_params.custom_extensions.push(subject_key_identifier_extension(&signing_key));

        // We create three signers with different validity dates
        cert_params.not_after = rcgen::date_time_ymd(2025, 7, 1);
        let cert_old =
            cert_params.self_signed(&signing_key).expect("Failed to generate certificate");
        let x509_signer_old = {
            let rust_raw_x509_signer =
                RustRawX509Signer::new_from_pem_data(&cert_old.pem(), &signing_key.serialize_pem())
                    .unwrap();
            X509Signer::new(Arc::new(rust_raw_x509_signer))
        };

        cert_params.not_after = rcgen::date_time_ymd(2026, 7, 1);
        let cert = cert_params.self_signed(&signing_key).expect("Failed to generate certificate");
        let x509_signer_current = {
            let rust_raw_x509_signer =
                RustRawX509Signer::new_from_pem_data(&cert.pem(), &signing_key.serialize_pem())
                    .unwrap();
            X509Signer::new(Arc::new(rust_raw_x509_signer))
        };

        cert_params.not_after = rcgen::date_time_ymd(2027, 7, 1);
        let cert_new =
            cert_params.self_signed(&signing_key).expect("Failed to generate certificate");
        let x509_signer_new = {
            let rust_raw_x509_signer =
                RustRawX509Signer::new_from_pem_data(&cert_new.pem(), &signing_key.serialize_pem())
                    .unwrap();
            X509Signer::new(Arc::new(rust_raw_x509_signer))
        };

        (x509_signer_old, x509_signer_current, x509_signer_new)
    }
}
