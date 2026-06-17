use std::{ops::Deref, sync::Arc};

use cms::{
    cert::CertificateChoices,
    content_info::{CmsVersion, ContentInfo},
    signed_data::{
        CertificateSet, EncapsulatedContentInfo, SignatureValue, SignedData, SignerIdentifier,
        SignerInfo, SignerInfos,
    },
};
use const_oid::db::rfc5911::ID_SIGNED_DATA;
use pkcs1::RsaPssParams;
use rustls::{
    RootCertStore, SignatureScheme,
    crypto::aws_lc_rs::default_provider,
    pki_types::{CertificateDer, PrivateKeyDer, UnixTime, pem::PemObject},
    server::WebPkiClientVerifier,
    sign::SigningKey,
};
use webpki::EndEntityCert;
use x509_cert::{
    der::{
        self as der, AnyRef, Decode, Encode, EncodePem, Length, PemReader, Writer,
        pem::{LineEnding, PemLabel},
    },
    ext::pkix::SubjectKeyIdentifier,
    spki::{AlgorithmIdentifier, AlgorithmIdentifierRef},
};

// TODO: how to load from a PKCS12 bundle? cms::encrypted_data, I think
const KEY_BUNDLE: &[u8] = include_bytes!("bundle.p12");

const CA_CERT: &[u8] = include_bytes!("cacert.pem");
const CERT_BUNDLE_PEM: &[u8] = include_bytes!("cert.pem");
const PRIVATE_KEY_PEM: &[u8] = include_bytes!("key.pem");

/*
openssl cms -sign -md sha512 -in ../../README.md -signer src/cert.pem -inkey src/key.pem \
  -certfile src/cacert.pem -outform PEM -keyopt rsa_padding_mode:pss  > src/cms.pem

openssl cms -verify -CAfile src/cacert.pem -inform PEM -in src/cms.pem -content ../../README.md
*/
const CMS_PEM: &[u8] = include_bytes!("cms.pem");

/*

{"signatures": {"@user:domain": {
  "x509:<key_id>": {
    "certificates": [
      "<pem-encoded-device-cert>",
      "<pem-encoded-intermediate-cert>"
    ],
    "signature_scheme": 0x806, // A number from https://www.iana.org/assignments/tls-parameters/tls-parameters.xhtml#tls-signaturescheme
    "signature": <...>
  }
}}}

 */

fn main() {
    // dump_cms_file(CMS_PEM);
    default_provider().install_default().expect("unable to install default provider");
    let sig = build_signature();

    let chain = x509_cert::Certificate::load_pem_chain(CERT_BUNDLE_PEM).unwrap();

    let leaf_cert = &chain[0].tbs_certificate;
    let (_critical, ski): (_, SubjectKeyIdentifier) = leaf_cert.get().unwrap().unwrap();

    let signature_params = RsaPssParams {
        hash: AlgorithmIdentifierRef {
            oid: const_oid::db::rfc5912::ID_SHA_512,
            parameters: Some(AnyRef::NULL),
        },
        mask_gen: AlgorithmIdentifier {
            oid: const_oid::db::rfc5912::ID_MGF_1,
            parameters: Some(AlgorithmIdentifierRef {
                oid: const_oid::db::rfc5912::ID_SHA_512,
                parameters: Some(AnyRef::NULL),
            }),
        },
        salt_len: 64,
        trailer_field: Default::default(),
    };

    let signer_info = SignerInfo {
        // RFC 5652 § 5.3: version is the syntax version number.  If the SignerIdentifier is
        // the CHOICE issuerAndSerialNumber, then the version MUST be 1. If
        // the SignerIdentifier is subjectKeyIdentifier, then the version MUST be 3.
        version: CmsVersion::V3,

        sid: SignerIdentifier::SubjectKeyIdentifier(ski),
        digest_alg: AlgorithmIdentifier {
            oid: const_oid::db::rfc5912::ID_SHA_512,
            parameters: None,
        },
        signed_attrs: None, // Not required if EncapsulatedContentInfo is id-data
        signature_algorithm: AlgorithmIdentifier {
            oid: const_oid::db::rfc5912::ID_RSASSA_PSS,
            parameters: Some(der::Any::encode_from(&signature_params).unwrap()),
        },
        signature: SignatureValue::new(sig).unwrap(),
        unsigned_attrs: None,
    };

    let certificates: Vec<_> = chain.into_iter().map(CertificateChoices::Certificate).collect();

    let digest_algorithms =
        vec![AlgorithmIdentifier { oid: const_oid::db::rfc5912::ID_SHA_512, parameters: None }];

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
        digest_algorithms: digest_algorithms.try_into().unwrap(),
        encap_content_info: EncapsulatedContentInfo {
            econtent_type: const_oid::db::rfc5911::ID_DATA,
            econtent: None,
        },
        certificates: Some(CertificateSet::try_from(certificates).unwrap()),
        crls: None,
        signer_infos: vec![signer_info].try_into().unwrap(),
    };

    let content_info = ContentInfo {
        content_type: ID_SIGNED_DATA,
        content: der::Any::encode_from(&signed_data).unwrap(),
    };
    let content_info = ContentInfoWrapper(content_info);
    print!("{}", content_info.to_pem(LineEnding::CRLF).unwrap());

    //content_info.to_pem();
    // let mut pem_writer = PemWriter::new().unwrap();
    // content_info.encode(&pem_writer)

    //verify(sig);
}

fn debug_cms_file(pem: &[u8]) {
    let mut pem_reader = PemReader::new(pem).unwrap();
    let data = ContentInfo::decode(&mut pem_reader).unwrap();
    dbg!(&data.content_type);

    let data: SignedData = data.content.decode_as().expect("Could not parse SignedData");
    // let data = SignedData::from_der(&data.content.value()).expect("Could not
    // parse SignedData");
    dbg!(data.version);
    dbg!(data.digest_algorithms);
    dbg!(data.encap_content_info);
    dbg!(data.certificates);
    dbg!(data.crls);

    let signer_info = data.signer_infos.0.get(0).unwrap();
    let signature_algorithm = &signer_info.signature_algorithm;
    let params = signature_algorithm.parameters.as_ref().unwrap();
    let params: RsaPssParams = params.decode_as().unwrap();
    dbg!(params);
    dbg!(data.signer_infos);
}

/// A wrapper for [`ContentInfo`] which implements [`PemLabel`] and can
/// therefore be used with [`EncodePem`].
struct ContentInfoWrapper(ContentInfo);

impl PemLabel for ContentInfoWrapper {
    const PEM_LABEL: &'static str = "CMS";
}
impl Encode for ContentInfoWrapper {
    fn encoded_len(&self) -> der::Result<Length> {
        self.0.encoded_len()
    }

    fn encode(&self, encoder: &mut impl Writer) -> der::Result<()> {
        self.0.encode(encoder)
    }
}

/// upload side
fn build_signature() -> Vec<u8> {
    let provider = default_provider();
    let private_key =
        PrivateKeyDer::from_pem_slice(PRIVATE_KEY_PEM).expect("unable to parse private key");
    let private_key: Arc<dyn SigningKey> =
        provider.key_provider.load_private_key(private_key).expect("unable to load private key");
    let signer = private_key
        .choose_scheme(&[SignatureScheme::RSA_PSS_SHA512])
        .expect("unable to choose signature scheme");
    signer.sign(b"hello world").expect("unable to sign")
}

/// verifier side
fn verify(sig: Vec<u8>) {
    // Load the trusted root certificates into a verifier. These would come from
    // local configuration.
    let mut root_store = RootCertStore::empty();
    for result in CertificateDer::pem_slice_iter(CA_CERT) {
        root_store
            .add(result.expect("Unable to parse certificate in root store"))
            .expect("Unable to add certificate to root store");
    }
    let cert_verifier = WebPkiClientVerifier::builder(Arc::new(root_store))
        //.with_crls(...)
        .build()
        .unwrap();

    // Verify the certificate chain presented by the device
    let mut cert_iter = CertificateDer::pem_slice_iter(CERT_BUNDLE_PEM)
        .map(|r| r.expect("Unable to parse cert in cert chain"));
    let end_cert = cert_iter.next().expect("Empty certificate chain");
    let intermediate_certs: Vec<_> = cert_iter.collect();

    cert_verifier
        .verify_client_cert(&end_cert, intermediate_certs.as_ref(), UnixTime::now())
        .expect("Unable to verify client certificate");

    use x509_parser::prelude::*;
    let (_, parsed_cert) =
        X509Certificate::from_der(end_cert.as_ref()).expect("Unable to parse end cert");
    let email = parsed_cert.subject.iter_email().next().expect("No email in subject");
    dbg!(&email.as_str().unwrap());

    let provider = default_provider();

    let alg = provider
        .signature_verification_algorithms
        .mapping
        .iter()
        .filter(|item| item.0 == SignatureScheme::RSA_PSS_SHA512)
        .filter_map(|item| item.1.get(0).map(|i| i.deref()))
        .next()
        .expect("Unable to find RSA_PSS_SHA512 in provider");

    let cert = EndEntityCert::try_from(&end_cert).expect("Unable to parse end entity cert");
    let subject = cert.subject();

    println!("subject: {:?}", vodozemac::base64_encode(subject));
    cert.verify_signature(alg, b"hello world", &sig).expect("Unable to verify signature");
}
