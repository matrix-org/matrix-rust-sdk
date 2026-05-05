use std::{ops::Deref, sync::Arc};

use rustls::{
    RootCertStore, SignatureScheme,
    crypto::aws_lc_rs::default_provider,
    pki_types::{CertificateDer, PrivateKeyDer, UnixTime, pem::PemObject},
    server::WebPkiClientVerifier,
    sign::SigningKey,
};
use webpki::EndEntityCert;

// TODO: how to load from a PKCS12 bundle? cms::encrypted_data, I think
const KEY_BUNDLE: &[u8] = include_bytes!("bundle.p12");

const CA_CERT: &[u8] = include_bytes!("cacert.pem");
const CERT_BUNDLE_PEM: &[u8] = include_bytes!("cert.pem");
const PRIVATE_KEY_PEM: &[u8] = include_bytes!("key.pem");

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
    default_provider().install_default().expect("unable to install default provider");
    let sig = build_signature();
    verify(sig);
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
