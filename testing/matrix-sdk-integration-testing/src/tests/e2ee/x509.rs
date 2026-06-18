use std::sync::Arc;

use matrix_sdk::encryption::EncryptionSettings;
use matrix_sdk_base::crypto::x509::{
    RustRawX509Signer, RustRawX509Verifier, X509Signer, X509Verifier,
};
use oid_registry::OID_PKCS9_EMAIL_ADDRESS;
use rand::RngExt as _;
use rcgen::{Certificate, CertificateParams, Issuer, KeyPair};
use tracing::Instrument as _;

use crate::helpers::{SyncTokenAwareClient, TestClientBuilder};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_user_is_verified_if_their_msk_is_signed() -> anyhow::Result<()> {
    let alice_span = tracing::info_span!("alice");
    let bob_span = tracing::info_span!("bob");

    let (alice_username, alice_email) = username_and_email("alice");

    // This is the happy path.
    //
    // Alice has a valid X.509 key pair signed by a CA that both Alice and Bob trust
    let (ca_cert, ca_signing_key) = ca_cert();

    let (alice_cert, alice_signing_key) = cert_and_key_with_email(&alice_email, &ca_signing_key);

    let alice_certs_pem = alice_cert.pem() + ca_cert.pem().as_str();
    let alice_key_pem = alice_signing_key.serialize_pem();

    let alice_x509_signer = {
        let rust_raw_x509_signer =
            RustRawX509Signer::new_from_pem_data(&alice_certs_pem, &alice_key_pem).unwrap();
        X509Signer::new(Arc::new(rust_raw_x509_signer))
    };

    let alice_x509_verifier = {
        let rust_raw_x509_verifier =
            RustRawX509Verifier::new_from_pem_data(&ca_cert.pem()).unwrap();
        X509Verifier::new(Arc::new(rust_raw_x509_verifier))
    };

    // Alice registers on the server. As part of creating their user identity, they
    // sign their MSK and upload the signature to the server (as part of
    // uploading their identity).
    let alice = create_encryption_enabled_client(
        &alice_username,
        Some(alice_x509_signer.clone()),
        Some(alice_x509_verifier.clone()),
    )
    .instrument(alice_span.clone())
    .await?;

    let (bob_username, bob_email) = username_and_email("bob");

    let (bob_cert, bob_signing_key) = cert_and_key_with_email(&bob_email, &ca_signing_key);

    let bob_certs_pem = bob_cert.pem() + ca_cert.pem().as_str();
    let bob_key_pem = bob_signing_key.serialize_pem();

    let bob_x509_signer = {
        let rust_raw_x509_signer =
            RustRawX509Signer::new_from_pem_data(&bob_certs_pem, &bob_key_pem).unwrap();
        X509Signer::new(Arc::new(rust_raw_x509_signer))
    };

    let bob_x509_verifier = {
        let rust_raw_x509_verifier =
            RustRawX509Verifier::new_from_pem_data(&ca_cert.pem()).unwrap();
        X509Verifier::new(Arc::new(rust_raw_x509_verifier))
    };

    // Bob logs in to the server.
    let bob = create_encryption_enabled_client(
        &bob_username,
        Some(bob_x509_signer.clone()),
        Some(bob_x509_verifier.clone()),
    )
    .instrument(bob_span.clone())
    .await?;

    // Bob sees Alice as trusted because her MSK is signed by a valid X.509 key.
    let bobs_view_of_alice =
        bob.encryption().request_user_identity(alice.user_id().unwrap()).await?.unwrap();

    assert!(bobs_view_of_alice.is_verified());

    Ok(())
}

fn username_and_email(prefix: &str) -> (String, String) {
    let suffix: u128 = rand::rng().random();

    let username = format!("{prefix}{suffix}");
    let email = format!("{username}@matrix-sdk.rs");

    (username, email)
}

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
    let alice_user_id = alice.user_id().unwrap();

    // Bob logs in to the server.
    let bob =
        create_encryption_enabled_client("bob", None, None).instrument(bob_span.clone()).await?;

    // Bob sees Alice as untrusted because her MSK is not signed by a valid X.509
    // key.
    let bobs_view_of_alice = bob.encryption().request_user_identity(alice_user_id).await?.unwrap();

    assert!(!bobs_view_of_alice.is_verified());

    Ok(())
}

// Additional tests:
//
// test_user_is_still_verified_after_they_reset_their_identity
// test_user_is_not_verified_if_we_dont_trust_their_cert
// test_user_is_not_verified_if_cert_is_expired

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

fn ca_cert() -> (Certificate, KeyPair) {
    let mut cert_params = CertificateParams::default();
    cert_params.use_authority_key_identifier_extension = true;

    let signing_key =
        KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

    (cert_params.self_signed(&signing_key).expect("Failed to generate certificate"), signing_key)
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
fn cert_and_key_with_email(email: &str, ca_signing_key: &KeyPair) -> (Certificate, KeyPair) {
    let mut cert_params = CertificateParams::default();
    //cert_params.use_authority_key_identifier_extension = true;
    cert_params.distinguished_name.push(
        rcgen::DnType::CustomDnType(OID_PKCS9_EMAIL_ADDRESS.iter().unwrap().collect()),
        email,
    );

    let signing_key =
        KeyPair::generate_for(&rcgen::PKCS_RSA_SHA512).expect("Failed to generate key pair");

    let issuer = Issuer::new(CertificateParams::default(), ca_signing_key);

    (
        cert_params.signed_by(&ca_signing_key, &issuer).expect("Failed to generate certificate"),
        signing_key,
    )
}
