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
            der::{AnyRef, EncodePem, asn1::SetOfVec, pem::LineEnding},
            ext::pkix::{AuthorityKeyIdentifier, SubjectKeyIdentifier},
            spki::{AlgorithmIdentifierOwned, AlgorithmIdentifierRef},
        },
    },
    content_info::{CmsVersion, ContentInfo},
    signed_data::{
        EncapsulatedContentInfo, SignatureValue, SignedData, SignerIdentifier, SignerInfo,
    },
};
use rsa::pkcs1::RsaPssParams;
use ruma::OwnedDeviceId;
use sha2::digest::const_oid;
use thiserror::Error;
use vodozemac::base64_encode;

#[cfg(doc)]
use crate::x509::{RawX509Signer, RawX509Verifier};
use crate::{SignatureError, types::X509Signature};

/// The object we receive from [`RawX509Signer`], and pass to
/// [`RawX509Verifier`].
///
/// A simplified representation of the data in the signature object.
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
    pub fn into_x509_signature(self) -> Result<(OwnedDeviceId, X509Signature), SignatureError> {
        let cert_chain = Certificate::load_pem_chain(self.certificate_chain.as_bytes())
            .expect("unable to parse cert chain returned by RawX509Signer");

        if cert_chain.is_empty() {
            panic!("Empty cert chain returned by RawX509Signer");
        }

        // The subject key identifier of the cert for the key that we signed with.
        let leaf_ski = cert_chain[0]
            .tbs_certificate
            .get::<SubjectKeyIdentifier>()
            .expect("Error parsing extensions in leaf cert")
            .expect("No SubjectKeyIdentifier in leaf cert")
            .1;

        // The authority key identifier of the highest cert in the chain, which should
        // be the subject key identifier of the CA.
        let authority_key_identifier = cert_chain
            .last()
            .unwrap()
            .tbs_certificate
            .get::<AuthorityKeyIdentifier>()
            .expect("Error parsing extensions in intermediate cert")
            .expect("No AuthorityKeyIdentifier in intermediate cert")
            .1;

        let signer_info = SignerInfo {
            // RFC 5652 § 5.3: version is the syntax version number.  If the SignerIdentifier is
            // the CHOICE issuerAndSerialNumber, then the version MUST be 1. If
            // the SignerIdentifier is subjectKeyIdentifier, then the version MUST be 3.
            version: CmsVersion::V3,

            sid: SignerIdentifier::SubjectKeyIdentifier(leaf_ski),
            digest_alg: self.signature_scheme.get_digest_algorithm(),
            signed_attrs: None, // Not required if EncapsulatedContentInfo is id-data
            signature_algorithm: self.signature_scheme.into(),
            signature: SignatureValue::new(self.signature_bytes).unwrap(),
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
            digest_algorithms: vec![signer_info.digest_alg.clone()].try_into().unwrap(),
            encap_content_info: EncapsulatedContentInfo {
                econtent_type: const_oid::db::rfc5911::ID_DATA,
                econtent: None,
            },
            certificates: Some(
                SetOfVec::from_iter(cert_chain.into_iter().map(CertificateChoices::Certificate))
                    .unwrap()
                    .into(),
            ),
            crls: None,
            signer_infos: vec![signer_info].try_into().unwrap(),
        };

        let signature = X509Signature::new(ContentInfo {
            content_type: const_oid::db::rfc5911::ID_SIGNED_DATA,
            content: der::Any::encode_from(&signed_data).unwrap(),
        });

        let device_id = OwnedDeviceId::from(base64_encode(
            authority_key_identifier
                .key_identifier
                .expect("No key identifier in AuthorityKeyIdentifier"),
        ));

        Ok((device_id, signature))
    }
}

/// Error type for [`RawX509Signature::try_from`].
#[derive(Debug, Error)]
pub enum ParseRawX509SignatureError {
    // TODO: populate
}

impl TryFrom<&X509Signature> for RawX509Signature {
    type Error = ParseRawX509SignatureError;

    fn try_from(value: &X509Signature) -> Result<Self, Self::Error> {
        let content_info = value.get_signature();
        if content_info.content_type != const_oid::db::rfc5911::ID_SIGNED_DATA {
            panic!("ContentInfo should be ID_SIGNED_DATA");
        }
        let data: SignedData =
            content_info.content.decode_as().expect("Could not parse SignedData");
        // TODO? check version?
        // TODO? check digest_algorithms?
        if data.encap_content_info.econtent_type != const_oid::db::rfc5911::ID_DATA {
            panic!("EncapsulatedContentInfo should be ID_DATA");
        }
        if data.encap_content_info.econtent.is_some() {
            panic!("EncapsulatedContentInfo.content should be None");
        }

        let certificates = data.certificates.expect("No certificates");
        let leaf_cert = match certificates.0.get(0).expect("Expected at least one certificate") {
            CertificateChoices::Certificate(c) => c,
            CertificateChoices::Other(_) => {
                panic!("CertificateChoices should be Certificate");
            }
        };
        let leaf_ski: SubjectKeyIdentifier = leaf_cert.tbs_certificate.get().unwrap().unwrap().1;

        let cert_pems = certificates
            .0
            .iter()
            .map(|cert| match cert {
                CertificateChoices::Certificate(c) => c.to_pem(LineEnding::CRLF).unwrap(),
                CertificateChoices::Other(_) => {
                    panic!("CertificateChoices should be Certificate");
                }
            })
            .collect::<Vec<_>>()
            .join("\r\n");

        if data.crls.is_some() {
            panic!("CRLs should be None");
        }

        let mut signer_infos = data.signer_infos.0.into_vec();
        let signer_info = signer_infos.pop().expect("SignerInfos was empty");
        if !signer_infos.is_empty() {
            panic!("SignerInfos contained multiple entries");
        }

        // TODO: check version?
        match &signer_info.sid {
            SignerIdentifier::IssuerAndSerialNumber(_) => {
                panic!("SignerIdentifier should be SubjectKeyIdentifier")
            }
            SignerIdentifier::SubjectKeyIdentifier(ski) => {
                if *ski != leaf_ski {
                    panic!("SignerIdentifier should match SubjectKeyIdentifier of leaf certificate")
                }
            }
        };

        check_sha512_algorithm_owned(&signer_info.digest_alg);

        if signer_info.signed_attrs.is_some() {
            panic!("SignerInfo.signed_attrs should be None");
        }

        signer_info
            .signature_algorithm
            .assert_algorithm_oid(const_oid::db::rfc5912::ID_RSASSA_PSS)
            .expect("SignerInfo.signature_algorithm.oid should be RSASSA_PSS");

        let params = signer_info
            .signature_algorithm
            .parameters
            .as_ref()
            .expect("SignerInfo.signature_algorithm.params should be set");
        let params: RsaPssParams<'_> = params
            .decode_as()
            .expect("Could not parse SignatureAlgorithm.parameters as RsaPssParams");

        check_sha512_algorithm_ref(params.hash);

        params
            .mask_gen
            .assert_algorithm_oid(const_oid::db::rfc5912::ID_MGF_1)
            .expect("RsaPssParams.mask_gen should be ID_MGF_1");

        check_sha512_algorithm_ref(
            params.mask_gen.parameters.expect("mask_gen.parameters must be set"),
        );

        if params.salt_len != 64 {
            panic!("salt_len should be 64");
        }

        Ok(RawX509Signature {
            signature_bytes: signer_info.signature.into_bytes(),
            certificate_chain: cert_pems,
            signature_scheme: X509SignatureScheme::RsaPssSha512,
        })
    }
}

fn check_sha512_algorithm_owned(hash_algorithm: &AlgorithmIdentifierOwned) {
    hash_algorithm
        .assert_algorithm_oid(const_oid::db::rfc5912::ID_SHA_512)
        .expect("hash.oid should be ID_SHA_512");

    match hash_algorithm.parameters {
        None => {}
        Some(_) => panic!("hash.parameters should be None"),
    }
}

fn check_sha512_algorithm_ref(hash_algorithm: AlgorithmIdentifierRef<'_>) {
    hash_algorithm
        .assert_algorithm_oid(const_oid::db::rfc5912::ID_SHA_512)
        .expect("hash.oid should be ID_SHA_512");

    match hash_algorithm.parameters {
        None => {}
        Some(AnyRef::NULL) => {}
        Some(_) => panic!("hash.parameters should be None"),
    }
}

/// The algorithm that was used to construct the signature.
///
/// This might be extended in future, but for now we only support
/// RsaPssSha512.
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
}

impl Into<AlgorithmIdentifierOwned> for X509SignatureScheme {
    /// Build an X.509 `AlgorithmIdentifier` (as defined in [RFC 5280 §
    /// 4.1.1.2]) for this signature scheme.
    ///
    /// [RFC 5280 § 4.1.1.2]: https://tools.ietf.org/html/rfc5280#section-4.1.1.2
    fn into(self) -> AlgorithmIdentifierOwned {
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
