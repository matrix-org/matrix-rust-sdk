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
