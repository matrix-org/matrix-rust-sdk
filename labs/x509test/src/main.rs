use std::{ops::Deref, sync::Arc};

use rustls::{
    RootCertStore, SignatureScheme,
    crypto::{CryptoProvider, aws_lc_rs},
    pki_types::{CertificateDer, PrivateKeyDer, UnixTime, pem::PemObject},
    server::WebPkiClientVerifier,
    sign::{Signer, SigningKey},
};

// TODO: how to load from a PKCS12 bundle? cms::encrypted_data, I think
const KEY_BUNDLE: &[u8] = include_bytes!("bundle.p12");

const CA_CERT: &[u8] = include_bytes!("cacert.pem");
const CERT_BUNDLE_PEM: &[u8] = include_bytes!("cert.pem");
const PRIVATE_KEY_PEM: &[u8] = include_bytes!("key.pem");

fn main() {
    aws_lc_rs::default_provider().install_default().expect("unable to install default provider");
    let sig = build_signature();
    verify(sig);
}

/// upload side
fn build_signature() -> Vec<u8> {
    let provider = CryptoProvider::get_default().expect("unable to get default provider");
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
    let verifier = WebPkiClientVerifier::builder(Arc::new(root_store))
        //.with_crls(...)
        .build()
        .unwrap();

    // Verify the certificate chain presented by the device
    let mut cert_iter = CertificateDer::pem_slice_iter(CERT_BUNDLE_PEM)
        .map(|r| r.expect("Unable to parse cert in cert chain"));
    let end_cert = cert_iter.next().expect("Empty certificate chain");
    let intermediate_certs: Vec<_> = cert_iter.collect();

    verifier
        .verify_client_cert(&end_cert, intermediate_certs.as_ref(), UnixTime::now())
        .expect("Unable to verify client certificate");

    // TODO: verify that the end cert is valid for the user id in question
    // TODO: verify signature from the end cert
}
