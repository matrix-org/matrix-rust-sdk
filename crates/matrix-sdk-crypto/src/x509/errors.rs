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

use std::error::Error;

use cms::cert::x509::{der, spki::ObjectIdentifier};
use thiserror::Error;

use crate::x509::raw_x509_signature::X509SignatureScheme;
#[cfg(doc)]
use crate::{types::X509Signature, x509::RawX509Signature};

/// An enumeration of errors that can occur when producing an X.509 signature.
#[derive(Error, Debug)]
pub enum X509SignatureSigningError {
    /// A [`RawX509Signature`] could not be converted into a [`X509Signature`].
    #[error("failed to convert from raw signature: {0}")]
    ConversionFailed(#[from] IntoX509SignatureError),

    /// A custom error specific to a particular implementation of
    /// [`crate::x509::RawX509Signer`] occurred.
    #[error(transparent)]
    Custom(Box<dyn Error + Send + Sync>),
}

/// An error that can occur while verifying an X.509 signature.
#[derive(Error, Debug)]
pub enum X509SignatureVerificationError {
    /// An [`X509Signature`] could not be converted into a [`RawX509Signature`].
    #[error("could not parse raw signature: {0}")]
    RawSignatureParseError(#[from] RawX509SignatureParseError),

    /// The certificate's user ID or equivalent email could not be verified.
    #[error("could not verify the certificate's email or user ID")]
    BadUserIdOrEmail,

    /// The certificate chain does not contain a first certificate, or it is
    /// invalid.
    #[error("missing or invalid first certificate")]
    MissingOrInvalidFirstCertificate,

    /// The certificate chain contains an invalid certificate.
    #[error("invalid certificate in chain")]
    InvalidCertificateInChain,

    /// The certificate has expired.
    #[error("certificate has expired")]
    CertificateExpired,

    /// The default [`rustls::crypto::CryptoProvider`] could not be accessed.
    #[error("unable to get default rustls crypto provider")]
    MissingCryptoProvider,

    /// The signature's scheme is unsupported.
    #[error("unsupported signature scheme: {0:?}")]
    UnsupportedSignatureScheme(X509SignatureScheme),

    /// A custom error specific to a particular implementation of
    /// [`crate::x509::RawX509Verifier`] occurred.
    #[error(transparent)]
    Custom(Box<dyn Error + Send + Sync>),
}

/// An error resulting from converting a [`RawX509Signature`] into an
/// [`X509Signature`].
#[derive(Error, Debug)]
pub enum IntoX509SignatureError {
    /// The certificate chain could not be parsed as DER.
    #[error("failed to parse certificate chain: {0}")]
    CertificateChainParseError(#[source] der::Error),

    /// The certificate chain contained no certificates.
    #[error("empty certificate chain")]
    EmptyCertChain,

    /// The extensions of the leaf certificate could not be parsed.
    #[error("failed to parse extensions in leaf certificate: {0}")]
    LeafCertificateExtensionParseError(#[source] der::Error),

    /// The extensions of the last certificate in the chain could not be parsed.
    #[error("failed to parse extensions in last certificate: {0}")]
    LastCertificateExtensionParseError(#[source] der::Error),

    /// The leaf certificate has no `SubjectKeyIdentifier` extension.
    #[error("no SubjectKeyIdentifier in leaf certificate")]
    LeafCertificateMissingSubjectKeyIdentifier,

    /// The last certificate has no `AuthorityKeyIdentifier` extension.
    #[error("no AuthorityKeyIdentifier in last certificate")]
    LastCertificateMissingAuthorityKeyIdentifier,

    /// The last certificate's `AuthorityKeyIdentifier` has no `KeyIdentifier`
    /// field.
    #[error("no KeyIdentifier in AuthorityKeyIdentifier in last certificate")]
    LastCertificateMissingKeyIdentifierInAuthorityKeyIdentifier,
}

/// An error resulting from checking the fields in an [`X509Signature`] when
/// converting to a [`RawX509Signature`].
#[derive(Error, Debug)]
pub enum RawX509SignatureParseError {
    /// The `ContentInfo` contentType was not `ID_SIGNED_DATA`.
    #[error("ContentInfo contentType is {}, not ID_SIGNED_DATA ({})", .0.actual, .0.expected)]
    UnexpectedContentInfoContentType(OidMismatch),

    /// The `EncapsulatedContentInfo` `eContentType` was not `ID_DATA`.
    #[error("EncapsulatedContentInfo eContentType is {}, not ID_DATA ({})", .0.actual, .0.expected)]
    UnexpectedEncapsulatedContentInfoContentType(OidMismatch),

    /// The `EncapsulatedContentInfo` `eContent` is not `NULL`.
    #[error("EncapsulatedContentInfo eContent is not NULL")]
    EncapsulatedContentNotNull,

    /// The `ContentInfo` content could not be parsed as a `SignedData`.
    #[error("could not parse content of ContentInfo as SignedData")]
    ContentInfoParseError(#[source] der::Error),

    /// The `SignedData` had no certificate chain.
    #[error("no certificate chain in SignedData")]
    NoCertificateChainInSignedData,

    /// The certificate chain in the `SignedData` was empty.
    #[error("empty certificate chain in SignedData")]
    EmptyCertificateChainInSignedData,

    /// The certificate chain contained a non-X.509 certificate.
    #[error("non-X.509 certificate in certificate chain")]
    NonX509CertificateInCertificateChain,

    /// The extensions of the leaf certificate could not be parsed.
    #[error("failed to parse extensions in leaf certificate: {0}")]
    LeafCertificateExtensionParseError(#[source] der::Error),

    /// The leaf certificate has no `SubjectKeyIdentifier` extension.
    #[error("no SubjectKeyIdentifier in leaf certificate")]
    LeafCertificateMissingSubjectKeyIdentifier,

    /// The `SignedData` contained certificate revocation lists, which are not
    /// allowed.
    #[error("SignedData contains certificate revocation lists")]
    SignedDataContainsCrls,

    /// The `signerInfos` list was empty.
    #[error("SignerInfos list is empty")]
    EmptySignerInfos,

    /// The `SignerInfos` list contained more than one entry.
    #[error("SignerInfos list contains multiple entries")]
    MultipleSignerInfos,

    /// The `SignerIdentifier` was not a `SubjectKeyIdentifier`.
    #[error("SignerIdentifier is not a SubjectKeyIdentifier")]
    SignerIdentifierNotSubjectKeyIdentifier,

    /// The `SignerIdentifier` did not match the leaf certificate's
    /// `SubjectKeyIdentifier`.
    #[error("SignerIdentifier does not match SubjectKeyIdentifier of leaf certificate")]
    SignerIdentifierMismatch,

    /// The `SignerInfo` contained signed attributes.
    #[error("SignerInfo contains signed attributes")]
    SignerInfoContainsSignedAttrs,

    // The signature algorithm is not supported.
    #[error("unsupported signature algorithm: {}", .0.actual)]
    UnsupportedSignatureAlgorithm(OidMismatch),

    /// The digest algorithm is not supported.
    #[error("unsupported digest algorithm: {}", .0.actual)]
    UnsupportedDigestAlgorithm(OidMismatch),

    /// The parameters to the digest algorithm are not NULL.
    #[error("digest algorithm parameters are not NULL")]
    DigestAlgorithmParametersNotNull,

    /// The `SignerInfo` signature algorithm parameters were not set.
    #[error("SignerInfo.signature_algorithm.parameters not set")]
    SignatureAlgorithmParametersNotSet,

    /// The `SignatureAlgorithm` parameters could not be parsed.
    #[error("could not parse SignatureAlgorithm.parameters")]
    SignatureAlgorithmParametersParseError(#[source] der::Error),

    /// The `SignatureAlgorithm` hash algorithm is not supported.
    #[error("unsupported SignatureAlgorithm hash algorithm: {}", .0.actual)]
    UnsupportedSignatureAlgorithmHash(OidMismatch),

    /// The `SignatureAlgorithm` mask generation function is not supported.
    #[error("unsupported SignatureAlgorithm mask generation function: {}", .0.actual)]
    UnsupportedSignatureAlgorithmMaskGen(OidMismatch),

    /// The `SignatureAlgorithm` mask generation function parameters were not
    /// set.
    #[error("SignatureAlgorithm mask generation function parameters not set")]
    SignatureAlgorithmMaskGenParametersNotSet,

    /// The `SignatureAlgorithm` mask generation function hash algorithm is not
    /// supported.
    #[error(
        "unsupported SignatureAlgorithm mask generation function hash algorithm: {}", .0.actual
    )]
    UnsupportedSignatureAlgorithmMaskGenHash(OidMismatch),

    /// The `SignatureAlgorithm` salt length is not `64`.
    #[error("SignatureAlgorithm salt length is not 64: {0}")]
    UnsupportedSignatureAlgorithmSaltLen(u8),

    /// One of the certificates in the chain could not be re-encoded into PEM.
    #[error("encoding certificate as PEM failed: {0}")]
    CertificatePemEncodingFailed(#[source] der::Error),
}

/// Helper type for some of the error codes in [`RawX509SignatureParseError`]:
/// reflects that an OID in the signature structure did not match the expected
/// value.
#[derive(Debug)]
pub struct OidMismatch {
    pub actual: ObjectIdentifier,
    pub expected: ObjectIdentifier,
}
