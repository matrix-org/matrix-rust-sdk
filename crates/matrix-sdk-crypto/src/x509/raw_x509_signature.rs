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

use cms::{
    cert::{
        CertificateChoices,
        x509::{
            Certificate, der,
            der::{
                EncodePem, asn1::SetOfVec, oid as const_oid, pem::LineEnding,
                referenced::OwnedToRef,
            },
            ext::pkix::{AuthorityKeyIdentifier, SubjectKeyIdentifier},
            spki::{AlgorithmIdentifierOwned, ObjectIdentifier},
        },
    },
    content_info::{CmsVersion, ContentInfo},
    signed_data::{
        EncapsulatedContentInfo, SignatureValue, SignedData, SignerIdentifier, SignerInfo,
        SignerInfos,
    },
};
use rsa::pkcs1::RsaPssParams;
use ruma::OwnedDeviceId;
use vodozemac::base64_encode;

#[cfg(doc)]
use crate::x509::{RawX509Signer, RawX509Verifier};
use crate::{
    types::X509Signature,
    x509::errors::{IntoX509SignatureError, OidMismatch, RawX509SignatureParseError},
};

/// The object we receive from [`RawX509Signer`], and pass to
/// [`RawX509Verifier`].
///
/// A simplified representation of the data in the signature object.
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug, Clone)]
pub struct RawX509Signature {
    /// The raw bytes of the signature.
    pub signature_bytes: Vec<u8>,

    /// The PEM-encoded certificate chain, starting with the device's own
    /// certificate, followed by intermediate certificates.
    pub certificate_chain: String,

    /// The algorithm that the signer used to construct the signature.
    pub signature_scheme: X509SignatureScheme,
}

impl RawX509Signature {
    /// Convert this signing result into an [`X509Signature`] containing a CMS
    /// `SignedData` object.
    pub fn into_x509_signature(
        self,
    ) -> Result<(OwnedDeviceId, X509Signature), IntoX509SignatureError> {
        let cert_chain = Certificate::load_pem_chain(self.certificate_chain.as_bytes())
            .map_err(IntoX509SignatureError::CertificateChainParseError)?;

        let (first_cert, last_cert) = match cert_chain.as_slice() {
            [] => return Err(IntoX509SignatureError::EmptyCertChain),
            [single] => (single, single),
            [first, .., last] => (first, last),
        };

        // The subject key identifier of the cert for the key that we signed with.
        let leaf_ski = first_cert
            .tbs_certificate
            .get::<SubjectKeyIdentifier>()
            .map_err(IntoX509SignatureError::LeafCertificateExtensionParseError)?
            .ok_or(IntoX509SignatureError::LeafCertificateMissingSubjectKeyIdentifier)?
            .1;

        // The authority key identifier of the highest cert in the chain, which should
        // be the subject key identifier of the CA.
        let authority_key_identifier = last_cert
            .tbs_certificate
            .get::<AuthorityKeyIdentifier>()
            .map_err(IntoX509SignatureError::LastCertificateExtensionParseError)?
            .ok_or(IntoX509SignatureError::LastCertificateMissingAuthorityKeyIdentifier)?
            .1;

        let authority_key_identifier_bytes = authority_key_identifier.key_identifier.ok_or(
            IntoX509SignatureError::LastCertificateMissingKeyIdenitifierInAuthorityKeyIdentifier,
        )?;

        let signer_info = SignerInfo {
            // RFC 5652 § 5.3: version is the syntax version number.  If the SignerIdentifier is
            // the CHOICE issuerAndSerialNumber, then the version MUST be 1. If
            // the SignerIdentifier is subjectKeyIdentifier, then the version MUST be 3.
            version: CmsVersion::V3,

            sid: SignerIdentifier::SubjectKeyIdentifier(leaf_ski),
            digest_alg: self.signature_scheme.get_digest_algorithm(),
            signed_attrs: None, // Not required if EncapsulatedContentInfo is id-data
            signature_algorithm: self.signature_scheme.get_signature_algorithm(),
            signature: SignatureValue::new(self.signature_bytes).expect(
                // This can only fail by failing to encode the length of the bytes as a usize
                "Unable to encode signature bytes as SignatureValue",
            ),
            unsigned_attrs: None,
        };

        let signed_data = SignedData {
            // RFC 5652 § 5.1.  SignedData Type
            // IF ((certificates is present) AND
            //             (any certificates with a type of other are present)) OR
            //             ((crls is present) AND
            //             (any crls with a type of other are present))
            //          THEN version MUST be 5
            //          ELSE
            //             IF (certificates is present) AND
            //                (any version 2 attribute certificates are present)
            //             THEN version MUST be 4
            //             ELSE
            //                IF ((certificates is present) AND
            //                   (any version 1 attribute certificates are present)) OR
            //                   (any SignerInfo structures are version 3) OR
            //                   (encapContentInfo eContentType is other than id-data)
            //                THEN version MUST be 3
            //                ELSE version MUST be 1
            //
            // TL;DR: since our SignerInfo is v3, we need a v3 SignedData.
            version: CmsVersion::V3,
            digest_algorithms: vec![signer_info.digest_alg.clone()].into_set_of_vec(),
            encap_content_info: EncapsulatedContentInfo {
                econtent_type: const_oid::db::rfc5911::ID_DATA,
                econtent: None,
            },
            certificates: Some(
                cert_chain
                    .into_iter()
                    .map(CertificateChoices::Certificate)
                    .collect::<Vec<_>>()
                    .into_set_of_vec()
                    .into(),
            ),
            crls: None,
            signer_infos: SignerInfos(vec![signer_info].into_set_of_vec()),
        };

        let signature = X509Signature::new(ContentInfo {
            content_type: const_oid::db::rfc5911::ID_SIGNED_DATA,
            content: der::Any::encode_from(&signed_data)
                .expect("Unable to encode SignedData as DER"),
        });

        let device_id = OwnedDeviceId::from(base64_encode(authority_key_identifier_bytes));

        Ok((device_id, signature))
    }
}

/// Helper trait for building [`SetOfVec`] whilst suppressing the impossible
/// error.
trait IntoSetOfVec<T: der::DerOrd> {
    fn into_set_of_vec(self) -> SetOfVec<T>;
}

impl<T: der::DerOrd> IntoSetOfVec<T> for Vec<T> {
    fn into_set_of_vec(self) -> SetOfVec<T> {
        // Building a SetOfVec has to calculate the lengths of each of the entries,
        // which is theoretically fallible if the lengths cannot be represented as a
        // `usize`.
        //
        // In practice, I can't see why it would fail.
        self.try_into().expect("Unable to construct SetOfVec")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RawX509SignatureAndFirstCertificate {
    pub raw_x509signature: RawX509Signature,
    pub leaf_cert: Certificate,
}

impl TryFrom<&X509Signature> for RawX509SignatureAndFirstCertificate {
    type Error = RawX509SignatureParseError;

    fn try_from(value: &X509Signature) -> Result<Self, Self::Error> {
        let content_info = value.get_signature();

        assert_oid_matches(&content_info.content_type, &const_oid::db::rfc5911::ID_SIGNED_DATA)
            .map_err(RawX509SignatureParseError::UnexpectedContentInfoContentType)?;

        let data: SignedData = content_info
            .content
            .decode_as()
            .map_err(RawX509SignatureParseError::ContentInfoParseError)?;

        data.try_into()
    }
}

impl TryFrom<SignedData> for RawX509SignatureAndFirstCertificate {
    type Error = RawX509SignatureParseError;

    fn try_from(data: SignedData) -> Result<Self, Self::Error> {
        // TODO? check version?
        // TODO? check digest_algorithms?
        let encapsulated_content_info = &data.encap_content_info;
        check_encapsulated_content_info(encapsulated_content_info)?;

        let certificates: Vec<_> = data
            .certificates
            .ok_or(RawX509SignatureParseError::NoCertificateChainInSignedData)?
            .0
            .into_vec()
            .into_iter()
            .map(|cert| match cert {
                CertificateChoices::Certificate(c) => Ok(c),
                CertificateChoices::Other(_) => {
                    Err(RawX509SignatureParseError::NonX509CertificateInCertificateChain)
                }
            })
            .collect::<Result<_, _>>()?;

        let cert_pems = certificates
            .iter()
            .map(|cert| {
                cert.to_pem(LineEnding::CRLF).expect("Unable to format certificate chain as PEMs")
            })
            .collect::<Vec<_>>()
            .join("");

        let leaf_cert = certificates
            .into_iter()
            .next()
            .ok_or(RawX509SignatureParseError::EmptyCertificateChainInSignedData)?;

        let leaf_ski = leaf_cert
            .tbs_certificate
            .get::<SubjectKeyIdentifier>()
            .map_err(RawX509SignatureParseError::LeafCertificateExtensionParseError)?
            .ok_or(RawX509SignatureParseError::LeafCertificateMissingSubjectKeyIdentifier)?
            .1;

        if data.crls.is_some() {
            return Err(RawX509SignatureParseError::SignedDataContainsCrls);
        }

        let mut signer_infos = data.signer_infos.0.into_vec();
        let signer_info = signer_infos.pop().ok_or(RawX509SignatureParseError::EmptySignerInfos)?;
        if !signer_infos.is_empty() {
            return Err(RawX509SignatureParseError::MultipleSignerInfos);
        }

        let (signature_scheme, signature_bytes) = parse_signer_info(signer_info, &leaf_ski)?;
        let raw_x509signature =
            RawX509Signature { signature_bytes, certificate_chain: cert_pems, signature_scheme };
        Ok(RawX509SignatureAndFirstCertificate { raw_x509signature, leaf_cert })
    }
}

fn check_encapsulated_content_info(
    encapsulated_content_info: &EncapsulatedContentInfo,
) -> Result<(), RawX509SignatureParseError> {
    assert_oid_matches(&encapsulated_content_info.econtent_type, &const_oid::db::rfc5911::ID_DATA)
        .map_err(RawX509SignatureParseError::UnexpectedEncapsulatedContentInfoContentType)?;
    if encapsulated_content_info.econtent.is_some() {
        return Err(RawX509SignatureParseError::EncapsulatedContentNotNull);
    }
    Ok(())
}

/// Verify the given [`SignerInfo`], checking its signer identifier matches
/// the given `expected_ski`.
///
/// # Returns
///
/// - The signature scheme used for the signature.
/// - The raw bytes of the signature.
fn parse_signer_info(
    signer_info: SignerInfo,
    expected_ski: &SubjectKeyIdentifier,
) -> Result<(X509SignatureScheme, Vec<u8>), RawX509SignatureParseError> {
    // TODO: check version?
    match &signer_info.sid {
        SignerIdentifier::IssuerAndSerialNumber(_) => {
            return Err(RawX509SignatureParseError::SignerIdentifierNotSubjectKeyIdentifier);
        }
        SignerIdentifier::SubjectKeyIdentifier(ski) => {
            if *ski != *expected_ski {
                return Err(RawX509SignatureParseError::SignerIdentifierMismatch);
            }
        }
    };

    if signer_info.signed_attrs.is_some() {
        return Err(RawX509SignatureParseError::SignerInfoContainsSignedAttrs);
    }

    let signature_scheme = map_signer_info_algorithms_to_signature_scheme(
        &signer_info.digest_alg,
        &signer_info.signature_algorithm,
    )?;

    let signature_bytes = signer_info.signature.into_bytes();
    Ok((signature_scheme, signature_bytes))
}

/// Given the digest and signature algorithms from a `SignerInfo` structure, map
/// them to one of our supported [`X509SignatureScheme`]s.
fn map_signer_info_algorithms_to_signature_scheme(
    digest_alg: &AlgorithmIdentifierOwned,
    signature_algorithm: &AlgorithmIdentifierOwned,
) -> Result<X509SignatureScheme, RawX509SignatureParseError> {
    // For now, we only support RsaPssSha512
    assert_oid_matches(&signature_algorithm.oid, &const_oid::db::rfc5912::ID_RSASSA_PSS)
        .map_err(RawX509SignatureParseError::UnsupportedSignatureAlgorithm)?;

    assert_oid_matches(&digest_alg.oid, &const_oid::db::rfc5912::ID_SHA_512)
        .map_err(RawX509SignatureParseError::UnsupportedDigestAlgorithm)?;
    assert_digest_alg_params_null_or_absent(&digest_alg.parameters.owned_to_ref())?;

    let signature_algorithm_params: RsaPssParams<'_> = signature_algorithm
        .parameters
        .as_ref()
        .ok_or(RawX509SignatureParseError::SignatureAlgorithmParametersNotSet)?
        .decode_as()
        .map_err(RawX509SignatureParseError::SignatureAlgorithmParametersParseError)?;

    assert_oid_matches(&signature_algorithm_params.hash.oid, &const_oid::db::rfc5912::ID_SHA_512)
        .map_err(RawX509SignatureParseError::UnsupportedSignatureAlgorithmHash)?;
    assert_digest_alg_params_null_or_absent(&signature_algorithm_params.hash.parameters)?;

    assert_oid_matches(&signature_algorithm_params.mask_gen.oid, &const_oid::db::rfc5912::ID_MGF_1)
        .map_err(RawX509SignatureParseError::UnsupportedSignatureAlgorithmMaskGen)?;

    let mask_gen_params = signature_algorithm_params
        .mask_gen
        .parameters
        .ok_or(RawX509SignatureParseError::SignatureAlgorithmMaskGenParametersNotSet)?;

    assert_oid_matches(&mask_gen_params.oid, &const_oid::db::rfc5912::ID_SHA_512)
        .map_err(RawX509SignatureParseError::UnsupportedSignatureAlgorithmMaskGenHash)?;
    assert_digest_alg_params_null_or_absent(&mask_gen_params.parameters)?;

    if signature_algorithm_params.salt_len != 64 {
        return Err(RawX509SignatureParseError::UnsupportedSignatureAlgorithmSaltLen(
            signature_algorithm_params.salt_len,
        ));
    }

    Ok(X509SignatureScheme::RsaPssSha512)
}

/// The algorithm that was used to construct the signature.
///
/// This might be extended in future, but for now we only support
/// RsaPssSha512.
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum X509SignatureScheme {
    /// SHA-512 message digest, with RSASSA-PSS signature scheme (aka RSA-PSS),
    /// per [RFC 8017 § 8.1].
    ///
    /// Not to be confused with RSA-PKCS1, which is incompatible.
    ///
    /// [RFC 8017 § 8.1]: https://www.rfc-editor.org/info/rfc8017/#section-8.1
    RsaPssSha512,
}

impl X509SignatureScheme {
    /// Build a CMS `DigestAlgorithmIdentifier` (as defined in [RFC 5652 §
    /// 4.1.1.2]) for the digest algorithm used by this signature algorithm.
    ///
    /// (Actually a `DigestAlgorithmIdentifier` is the same as a regular
    /// `AlgorithmIdentifier`, though obviously we expect it to identify a
    /// digest algorithm like SHA-512.)
    pub fn get_digest_algorithm(&self) -> AlgorithmIdentifierOwned {
        use der::oid::AssociatedOid;

        match self {
            X509SignatureScheme::RsaPssSha512 => AlgorithmIdentifierOwned {
                oid: sha2::Sha512::OID,
                parameters: Some(der::Any::null()),
            },
        }
    }

    /// Build an X.509 `AlgorithmIdentifier` (as defined in [RFC 5280 §
    /// 4.1.1.2]) for this signature scheme.
    ///
    /// [RFC 5280 § 4.1.1.2]: https://tools.ietf.org/html/rfc5280#section-4.1.1.2
    pub fn get_signature_algorithm(&self) -> AlgorithmIdentifierOwned {
        match self {
            X509SignatureScheme::RsaPssSha512 => {
                // If you are interested in the details of all the fields in RsaPssParams,
                // then https://crypto.stackexchange.com/a/58708 is a decent primer.
                // Fortunately the RustCrypto folks have done the legwork for us.
                rsa::pss::get_default_pss_signature_algo_id::<sha2::Sha512>()
                    .expect("Unable to get AlgorithmIdentifier for RSA_PSS")
            }
        }
    }
}

fn assert_oid_matches(
    actual: &ObjectIdentifier,
    expected: &ObjectIdentifier,
) -> Result<(), OidMismatch> {
    if *actual == *expected {
        Ok(())
    } else {
        Err(OidMismatch { actual: actual.clone(), expected: expected.clone() })
    }
}

fn assert_digest_alg_params_null_or_absent(
    digest_alg_params: &Option<der::AnyRef<'_>>,
) -> Result<(), RawX509SignatureParseError> {
    match digest_alg_params {
        None => Ok(()),
        Some(x) if x.is_null() => Ok(()),
        Some(_) => Err(RawX509SignatureParseError::DigestAlgorithmParametersNotNull),
    }
}

#[cfg(test)]
mod test {
    use insta::assert_debug_snapshot;

    use super::*;

    /// Test parsing a known CMS structure into a
    /// [`RawX509SignatureAndFirstCertificate`], and compare it against a
    /// snapshot.
    #[test]
    fn test_sig_to_raw() {
        const SIG: &str = include_str!("test_cms.pem");
        let x509signature = X509Signature::from_str(SIG).unwrap();
        let raw_signature: RawX509SignatureAndFirstCertificate =
            (&x509signature).try_into().unwrap();
        assert_debug_snapshot!(raw_signature);
    }

    /// We should be able to roundtrip a CMS structure into a
    /// [`RawX509Signature`] and back again.
    #[test]
    fn test_roundtrip_sig_to_raw_and_back() {
        const SIG: &str = include_str!("test_cms.pem");
        let x509signature = X509Signature::from_str(SIG).unwrap();
        let raw_and_certs: RawX509SignatureAndFirstCertificate =
            (&x509signature).try_into().unwrap();
        let (device_id, roundtripped) =
            raw_and_certs.raw_x509signature.into_x509_signature().unwrap();
        assert_eq!(roundtripped.to_string(), x509signature.to_string());

        // The SKI of the CA cert is
        // D8:5E:91:9A:17:F0:C3:5B:13:DB:75:42:7D:21:37:9A:DF:3E:96:11, which when
        // base64-encoded is...
        assert_eq!(device_id, "2F6Rmhfww1sT23VCfSE3mt8+lhE");
    }
}
