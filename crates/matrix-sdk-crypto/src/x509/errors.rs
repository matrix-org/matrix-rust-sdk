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

use cms::cert::x509::{der, spki::ObjectIdentifier};
use thiserror::Error;

#[cfg(doc)]
use crate::{types::X509Signature, x509::RawX509Signature};

/// An error resulting from converting a [`RawX509Signature`] into an
/// [`X509Signature`].
#[derive(Error, Debug)]
pub enum IntoX509SignatureError {
    #[error("failed to parse certificate chain: {0}")]
    CertificateChainParseError(#[source] der::Error),

    #[error("empty certificate chain")]
    EmptyCertChain,

    #[error("failed to parse extensions in leaf certificate: {0}")]
    LeafCertificateExtensionParseError(#[source] der::Error),

    #[error("failed to parse extensions in last certificate: {0}")]
    LastCertificateExtensionParseError(#[source] der::Error),

    #[error("no SubjectKeyIdentifier in leaf certificate")]
    LeafCertificateMissingSubjectKeyIdentifier,

    #[error("no AuthorityKeyIdentifier in last certificate")]
    LastCertificateMissingAuthorityKeyIdentifier,

    #[error("no KeyIdentifier in AuthorityKeyIdentifier in last certificate")]
    LastCertificateMissingKeyIdenitifierInAuthorityKeyIdentifier,
}

/// An error resulting from checking the fields in an [`X509Signature`] when
/// converting to a [`RawX509Signature`].
#[derive(Error, Debug)]
pub enum RawX509SignatureParseError {
    #[error("ContentInfo contentType is {}, not ID_SIGNED_DATA ({})", .0.actual, .0.expected)]
    UnexpectedContentInfoContentType(OidMismatch),

    #[error("EncapsulatedContentInfo eContentType is {}, not ID_DATA ({})", .0.actual, .0.expected)]
    UnexpectedEncapsulatedContentInfoContentType(OidMismatch),

    #[error("EncapsulatedContentInfo eContent is not NULL")]
    EncapsulatedContentNotNull,

    #[error("could not parse content of ContentInfo as SignedData")]
    ContentInfoParseError(#[source] der::Error),

    #[error("no certificate chain in SignedData")]
    NoCertificateChainInSignedData,

    #[error("empty certificate chain in SignedData")]
    EmptyCertificateChainInSignedData,

    #[error("non-X.509 certificate in certificate chain")]
    NonX509CertificateInCertificateChain,

    #[error("failed to parse extensions in leaf certificate: {0}")]
    LeafCertificateExtensionParseError(#[source] der::Error),

    #[error("no SubjectKeyIdentifier in leaf certificate")]
    LeafCertificateMissingSubjectKeyIdentifier,

    #[error("SignedData contains certificate revocation lists")]
    SignedDataContainsCrls,

    #[error("SignerInfos list is empty")]
    EmptySignerInfos,

    #[error("SignerInfos list contains multiple entries")]
    MultipleSignerInfos,

    #[error("SignerIdentifier is not a SubjectKeyIdentifier")]
    SignerIdentifierNotSubjectKeyIdentifier,

    #[error("SignerIdentifier does not match SubjectKeyIdentifier of leaf certificate")]
    SignerIdentifierMismatch,

    #[error("SignerInfo contains signed attributes")]
    SignerInfoContainsSignedAttrs,

    #[error("unsupported signature algorithm: {}", .0.actual)]
    UnsupportedSignatureAlgorithm(OidMismatch),

    #[error("unsupported digest algorithm: {}", .0.actual)]
    UnsupportedDigestAlgorithm(OidMismatch),

    #[error("digest algorithm parameters are not NULL")]
    DigestAlgorithmParametersNotNull,

    #[error("SignerInfo.signature_algorithm.parameters not set")]
    SignatureAlgorithmParametersNotSet,

    #[error("could not parse SignatureAlgorithm.parameters")]
    SignatureAlgorithmParametersParseError(#[source] der::Error),

    #[error("unsupported SignatureAlgorithm hash algorithm: {}", .0.actual)]
    UnsupportedSignatureAlgorithmHash(OidMismatch),

    #[error("unsupported SignatureAlgorithm mask generation function: {}", .0.actual)]
    UnsupportedSignatureAlgorithmMaskGen(OidMismatch),

    #[error("SignatureAlgorithm mask generation function parameters not set")]
    SignatureAlgorithmMaskGenParametersNotSet,

    #[error(
        "unsupported SignatureAlgorithm mask generation function hash algorithm: {}", .0.actual
    )]
    UnsupportedSignatureAlgorithmMaskGenHash(OidMismatch),

    #[error("SignatureAlgorithm salt length is not 64: {0}")]
    UnsupportedSignatureAlgorithmSaltLen(u8),
}

/// Helper type for some of the error codes in [`RawX509SignatureParseError`]:
/// reflects that an OID in the signature structure did not match the expected
/// value.
#[derive(Debug)]
pub struct OidMismatch {
    pub actual: ObjectIdentifier,
    pub expected: ObjectIdentifier,
}
