use std::sync::Arc;

use matrix_sdk::encryption::EncryptionSettings;
use matrix_sdk_crypto::x509::{RustRawX509Signer, RustRawX509Verifier, X509Signer, X509Verifier};
use oid_registry::OID_PKCS9_EMAIL_ADDRESS;
use rand::RngExt as _;
use rcgen::{
    Certificate, CertificateParams, CustomExtension, DnType, Issuer, KeyPair, PublicKeyData,
};
use tracing::Instrument as _;

use crate::helpers::{SyncTokenAwareClient, TestClientBuilder};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_user_is_not_verified_if_their_msk_is_not_signed() -> anyhow::Result<()> {
    let alice_span = tracing::info_span!("alice");
    let bob_span = tracing::info_span!("bob");

    // This is the base case: with no X.509 code running at all, users are
    // unverified.
    //
    // Alice has no X.509 key pair.
    // Alice registers on the server. They do not sign the MSK with any X.509 key.
    let alice = create_encryption_enabled_client("alice", None, None)
        .instrument(alice_span.clone())
        .await?;

    // Bob logs in to the server.
    let bob =
        create_encryption_enabled_client("bob", None, None).instrument(bob_span.clone()).await?;

    // Bob sees Alice as untrusted because her MSK is not signed by a valid X.509
    // key.
    let bobs_view_of_alice =
        bob.encryption().request_user_identity(alice.user_id().unwrap()).await?.unwrap();

    assert!(!bobs_view_of_alice.is_verified());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_user_is_verified_if_their_msk_is_signed() -> anyhow::Result<()> {
    let alice_span = tracing::info_span!("alice");
    let bob_span = tracing::info_span!("bob");

    let (alice_username, alice_email) = username_and_email("alice");

    // This is the happy path.
    //
    // A CA exists
    let (ca_cert, ca_signing_key) = ca_cert();

    // Alice has a valid X.509 key pair signed by the CA
    let (alice_cert, alice_signing_key) =
        cert_and_key_with_email_signed_by(&alice_email, &ca_cert, &ca_signing_key);

    // Alice can sign things using their key
    let alice_x509_signer = X509Signer::new(Arc::new(
        RustRawX509Signer::new_from_pem_data(&alice_cert.pem(), &alice_signing_key.serialize_pem())
            .unwrap(),
    ));

    // Alice registers on the server. As part of creating their user identity, they
    // sign their MSK and upload the signature to the server.
    let alice =
        create_encryption_enabled_client(&alice_username, Some(alice_x509_signer.clone()), None)
            .instrument(alice_span.clone())
            .await?;

    // Bob can verify signatures that are properly linked to the CA
    let (bob_username, _bob_email) = username_and_email("bob");
    let bob_x509_verifier = X509Verifier::new(Arc::new(
        RustRawX509Verifier::new_from_pem_data(&ca_cert.pem()).unwrap(),
    ));

    // Bob logs in to the server.
    let bob =
        create_encryption_enabled_client(&bob_username, None, Some(bob_x509_verifier.clone()))
            .instrument(bob_span.clone())
            .await?;

    // Bob sees Alice as trusted because her MSK is signed by a valid X.509 key.
    let bobs_view_of_alice =
        bob.encryption().request_user_identity(alice.user_id().unwrap()).await?.unwrap();

    assert!(bobs_view_of_alice.is_verified());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_user_is_not_verified_if_we_dont_trust_their_cert() -> anyhow::Result<()> {
    let alice_span = tracing::info_span!("alice");
    let bob_span = tracing::info_span!("bob");

    let (alice_username, alice_email) = username_and_email("alice");

    // This is a failure case: Alice and Bob know about X.509, but they use a
    // different CA, so Bob doesn't trust Alice's signature.
    //
    // 2 different CAs exist: one for Bob and one for Alice
    let (alice_ca_cert, alice_ca_signing_key) = ca_cert();
    let (bob_ca_cert, _bob_ca_signing_key) = ca_cert();

    // Alice has a valid X.509 key pair signed by their CA
    let (alice_cert, alice_signing_key) =
        cert_and_key_with_email_signed_by(&alice_email, &alice_ca_cert, &alice_ca_signing_key);

    // Alice can sign things using their key
    let alice_x509_signer = X509Signer::new(Arc::new(
        RustRawX509Signer::new_from_pem_data(&alice_cert.pem(), &alice_signing_key.serialize_pem())
            .unwrap(),
    ));

    // Alice registers on the server. As part of creating their user identity, they
    // sign their MSK and upload the signature to the server.
    let alice =
        create_encryption_enabled_client(&alice_username, Some(alice_x509_signer.clone()), None)
            .instrument(alice_span.clone())
            .await?;

    // Bob can verify signatures that are properly linked to their CA
    let (bob_username, _bob_email) = username_and_email("bob");
    let bob_x509_verifier = X509Verifier::new(Arc::new(
        RustRawX509Verifier::new_from_pem_data(&bob_ca_cert.pem()).unwrap(),
    ));

    // Bob logs in to the server.
    let bob =
        create_encryption_enabled_client(&bob_username, None, Some(bob_x509_verifier.clone()))
            .instrument(bob_span.clone())
            .await?;

    // Bob sees Alice as unverified because even though her MSK is signed by an
    // X.509 key, that key is not trusted because it is signed by a CA that Bob
    // does not know.
    let bobs_view_of_alice =
        bob.encryption().request_user_identity(alice.user_id().unwrap()).await?.unwrap();

    assert!(!bobs_view_of_alice.is_verified());

    Ok(())
}

/// Creates a new encryption-enabled client with the given username and
/// settings.
///
/// # Arguments
///
/// * `username` - The username for the client.
/// * `exclude_insecure_devices` - A boolean indicating whether to exclude
///   insecure devices.
async fn create_encryption_enabled_client(
    username: &str,
    x509_signer: Option<X509Signer>,
    x509_verifier: Option<X509Verifier>,
) -> anyhow::Result<SyncTokenAwareClient> {
    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

    let client = SyncTokenAwareClient::new(
        TestClientBuilder::with_exact_username(username.to_owned())
            .use_sqlite()
            .encryption_settings(encryption_settings)
            .x509_signer(x509_signer)
            .x509_verifier(x509_verifier)
            .build()
            .await?,
    );

    client.encryption().wait_for_e2ee_initialization_tasks().await;
    Ok(client)
}

/// Generate a little certificate authority i.e. a key pair and a
/// self-signed certificate.
fn ca_cert() -> (Certificate, KeyPair) {
    let cert_params = cert_params("Delboy Inc Trust Us Certificate Authority");

    let signing_key =
        KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

    (cert_params.self_signed(&signing_key).expect("Failed to generate certificate"), signing_key)
}

/// Create a certificate that contains the supplied email address in its
/// Subject Distinguished Name, and is signed by the supplied certificate
/// authority.
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
fn cert_and_key_with_email_signed_by(
    email: &str,
    ca_cert: &Certificate,
    ca_signing_key: &KeyPair,
) -> (Certificate, KeyPair) {
    let signing_key =
        KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

    let mut cert_params = cert_params(&format!("A nice cert for {email}"));
    let email_address_oid = OID_PKCS9_EMAIL_ADDRESS.iter().unwrap().collect();
    cert_params.distinguished_name.push(DnType::CustomDnType(email_address_oid), email);
    cert_params.custom_extensions.push(subject_key_identifier_extension(&signing_key));

    let issuer = Issuer::from_ca_cert_pem(&ca_cert.pem(), ca_signing_key)
        .expect("Failed to create an issuer for the CA");

    let cert =
        cert_params.signed_by(&signing_key, &issuer).expect("Failed to generate certificate");

    (cert, signing_key)
}

/// Create a CertificateParams (for creating a certificate) where the
/// distinguished name has the CommonName provided.
fn cert_params(common_name: &str) -> CertificateParams {
    let mut cert_params = CertificateParams::default();
    cert_params.distinguished_name.remove(DnType::CommonName);
    cert_params.distinguished_name.push(DnType::CommonName, common_name);
    cert_params.use_authority_key_identifier_extension = true;
    cert_params
}

fn username_and_email(prefix: &str) -> (String, String) {
    let suffix: u128 = rand::rng().random();

    let username = format!("{prefix}{suffix}");
    let email = format!("{username}@matrix-sdk.rs");

    (username, email)
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
