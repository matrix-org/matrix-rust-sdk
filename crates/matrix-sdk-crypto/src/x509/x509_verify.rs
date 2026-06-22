use std::{fmt::Debug, sync::Arc};

use ruma::{MatrixUri, OwnedUserId, UserId, matrix_uri::MatrixId};
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tracing::info;
use x509_parser::{asn1_rs::FromDer as _, certificate::X509Certificate, extensions::GeneralName};

use crate::{
    olm::SignedJsonObject,
    types::{Signature, X509Signature},
};

/// Hold one of these if you want to verify X.509 signatures, and call
/// [`Self::verify_x509_signature`] to do it.
///
/// Internally, this holds an implementation of [`RawX509Verifier`] that does
/// the real work of verifying things. This struct provides a convenient
/// wrapper.
#[derive(Debug, Clone)]
pub struct X509Verifier {
    x509_verify: Arc<dyn RawX509Verifier>,
}

impl X509Verifier {
    /// Create a new `X509Verifier` that wraps the supplied [`RawX509Verifier`].
    pub fn new(x509_verify: Arc<dyn RawX509Verifier>) -> X509Verifier {
        X509Verifier { x509_verify }
    }

    /// Verify that the given object is signed with a certificate issued by a
    /// trusted CA, and that the certificate was issued to the given user
    /// ID.
    pub(crate) fn verify_signed_object(
        &self,
        user_id: &UserId,
        signed_object: &(impl SignedJsonObject + Debug),
    ) -> bool {
        info!(?signed_object, "X509: verify_signed_object()");

        let Some(this_user_sigs) = signed_object.signatures().get(user_id) else {
            info!("X509: verify_signed_object(): no signatures on object");
            return false;
        };

        let Ok(msg) = signed_object.to_canonical_json() else {
            tracing::warn!("Unable to serialize object");
            return false;
        };

        for (_key_id, sig) in this_user_sigs {
            if let Ok(sig) = sig {
                if self.verify_x509_signature(user_id, &msg, sig) {
                    info!("X509: verify_signed_object(): verified X509 signature");
                    return true;
                } else {
                    tracing::warn!(
                        "X509: verify_signed_object(): X509 signature failed verification"
                    );
                }
            }
        }
        false
    }

    /// Check if the given signature is a valid X.509 signature for the given
    /// message.
    ///
    /// Also validates that the certificate used for the signature is issued via
    /// one of our trusted CAs, and was issued to the given user id.
    pub fn verify_x509_signature(&self, user_id: &UserId, message: &str, sig: &Signature) -> bool {
        let Signature::X509(sig) = sig else {
            // Not an error: just the wrong type of signature.
            return false;
        };

        // Before we pass over to the X.509 certificate verifier, check that the leaf
        // certificate is valid for the given user_id.
        let mut cert_iter = CertificateDer::pem_slice_iter(sig.certificate_chain.as_bytes());
        let Some(Ok(end_cert)) = cert_iter.next() else {
            tracing::warn!("Missing or invalid first certificate");
            return false;
        };

        if !cert_contains_user_id_or_equivalent_email(user_id, end_cert) {
            tracing::warn!("Verifying certificate user ID or email failed");
            return false;
        }

        self.x509_verify.verify(message.as_bytes(), sig)
    }
}

fn cert_contains_user_id_or_equivalent_email(
    user_id: &UserId,
    certificate: CertificateDer<'_>,
) -> bool {
    // Parse the certificate
    let Ok((_, parsed_cert)) = X509Certificate::from_der(certificate.as_ref()) else {
        tracing::warn!("Unable to parse certificate");
        return false;
    };

    // Check for a user ID in its SAN
    if let Some(certificate_user_id) = get_user_id_from_certificate(&parsed_cert) {
        if certificate_user_id == user_id {
            return true;
        } else {
            tracing::warn!(
                "Certificate not valid for this user. \
                Certificate user ID: {certificate_user_id}, \
                User ID: {user_id}",
            );
            return false;
        }
    }

    // Otherwise, as a fallback, check for an email address

    tracing::info!("Certificate subject does not contain a user ID. Checking for email address");

    let Some(certificate_email) = get_email_address_from_certificate(&parsed_cert) else {
        tracing::warn!("Certificate subject does not contain an email address");
        return false;
    };

    let expected_email = map_user_id_to_email(user_id);
    if certificate_email == expected_email {
        true
    } else {
        tracing::warn!(
            "Certificate not valid for this user. \
                Certificate email: {certificate_email}, \
                Expected email: {expected_email}, User ID: {user_id}",
        );
        false
    }
}

/// Something that can verify an X.509 signature.
pub trait RawX509Verifier: Debug + Send + Sync {
    /// Check if the given signature is a valid X.509 signature for the given
    /// message.
    ///
    /// Also validates that the certificate used for the signature is issued via
    /// one of our trusted CAs.
    fn verify(&self, message: &[u8], sig: &X509Signature) -> bool;
}

fn map_user_id_to_email(user_id: &UserId) -> String {
    // TODO RAV: this is not a reliable way to map from user_ids to email addresses.
    format!("{}@{}", user_id.localpart(), user_id.server_name())
}

/// Search this certificate's Subject Alternative Name for a URI that matches
/// the format of a Matrix URI that contains a valid Matrix user ID.
fn get_user_id_from_certificate(certificate: &X509Certificate<'_>) -> Option<OwnedUserId> {
    // If we have no SAN or SAN is not understood here, we definitely can't find a
    // user ID.
    let Ok(Some(san)) = certificate.subject_alternative_name() else {
        return None;
    };

    /// Check whether a SAN contains a valid Matrix user ID
    fn matrix_user_uri(alt_name: &GeneralName<'_>) -> Option<OwnedUserId> {
        // If it's a URI SAN type
        if let GeneralName::URI(uri) = alt_name {
            // And it parses as a `matrix:...` URI
            if let Ok(matrix_uri) = MatrixUri::parse(uri) {
                // And it's a user URI that produces a valid Matrix user ID
                if let MatrixId::User(user_id) = matrix_uri.id() {
                    // Then return it
                    return Some(user_id.clone());
                }
            }
        }

        // Otherwise, we didn't find a user ID
        None
    }

    // If any name looks right, return it - otherwise None
    san.value.general_names.iter().find_map(matrix_user_uri)
}

fn get_email_address_from_certificate(certificate: &X509Certificate<'_>) -> Option<String> {
    // Check for an email address in the Subject Alternative Name
    if let Ok(Some(san)) = certificate.subject_alternative_name() {
        if let Some(email) =
            san.value.general_names.iter().find_map(|n| {
                if let GeneralName::RFC822Name(email) = n { Some(email) } else { None }
            })
        {
            return Some((*email).to_owned());
        }
    }

    // Otherwise, check for the (legacy) email address in the Subject
    // Distinguished Name
    if let Some(email) = certificate.subject.iter_email().next() {
        return email.as_str().ok().map(ToOwned::to_owned);
    }

    // Otherwise, nothing was found
    None
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use rcgen::generate_simple_self_signed;
    use ruma::{DeviceKeyAlgorithm, DeviceKeyId, encryption::KeyUsage, user_id};
    use vodozemac::Ed25519SecretKey;

    use super::*;
    use crate::{
        types::{CrossSigningKey, SigningKeys},
        x509::{
            X509Signer,
            rust_raw_x509_signer::RustRawX509Signer,
            rust_raw_x509_verifier::RustRawX509Verifier,
            tests::{
                cert_and_key_with_email_in_subject_alternate_name,
                cert_and_key_with_email_in_subject_distinguished_name,
                cert_and_key_with_no_user_id, cert_and_key_with_user_id_in_subject_alternate_name,
            },
        },
    };

    #[test]
    fn test_can_extract_email_address_from_a_cert_sdn() {
        // Given a certificate containing an email address in the Subject
        // Distinguished Name
        let (cert, _) =
            cert_and_key_with_email_in_subject_distinguished_name("myname@company.co.uk");

        // When we extract the email address it contains
        let email =
            get_email_address_from_certificate(&X509Certificate::from_der(cert.der()).unwrap().1)
                .expect("Failed to get email address from cert");

        // Then it matches what we put in
        assert_eq!(email, "myname@company.co.uk");
    }

    #[test]
    fn test_can_extract_email_address_from_a_cert_san() {
        // Given a certificate containing an email address in the Subject
        // Alternative Name
        let (cert, _) = cert_and_key_with_email_in_subject_alternate_name("myname@company.co.uk");

        // When we extract the email address it contains
        let email =
            get_email_address_from_certificate(&X509Certificate::from_der(cert.der()).unwrap().1)
                .expect("Failed to get email address from cert");

        // Then it matches what we put in
        assert_eq!(email, "myname@company.co.uk");
    }

    #[test]
    fn test_can_extract_user_id_from_a_cert_san() {
        // Given a certificate containing a user ID in the Subject Alternative
        // Name
        let (cert, _) =
            cert_and_key_with_user_id_in_subject_alternate_name("@myname:company.co.uk");

        // When we extract the user ID it contains
        let user_id =
            get_user_id_from_certificate(&X509Certificate::from_der(cert.der()).unwrap().1)
                .expect("Failed to get email address from cert");

        // Then it matches what we put in
        assert_eq!(user_id, "@myname:company.co.uk");
    }

    #[test]
    fn test_extract_email_address_from_a_cert_that_does_not_contain_one_returns_none() {
        // Given a certificate not containing an email address
        let cert = generate_simple_self_signed(&[]).expect("Failed to generate cert");

        // When we attempt to extract the email address
        let email = get_email_address_from_certificate(
            &X509Certificate::from_der(cert.cert.der()).unwrap().1,
        );

        // Then the answer is empty
        assert!(email.is_none());
    }

    #[test]
    fn test_extract_user_id_from_a_cert_that_does_not_contain_one_returns_none() {
        // Given a certificate not containing an email address
        let cert = generate_simple_self_signed(&[]).expect("Failed to generate cert");

        // When we attempt to extract the email address
        let user_id =
            get_user_id_from_certificate(&X509Certificate::from_der(cert.cert.der()).unwrap().1);

        // Then the answer is empty
        assert!(user_id.is_none());
    }

    #[test]
    fn test_can_verify_cert_containing_email_in_dn() {
        // Given a cert containing the email address in the Subject Distinguished Name
        let (cert, signing_key) =
            cert_and_key_with_email_in_subject_distinguished_name("alice@localhost");

        // And a cross-signing key
        let (x509_signer, x509_verifier) = create_signer_and_verifier(cert, signing_key);

        let user_id = user_id!("@alice:localhost").to_owned();
        let mut cross_signing_key = create_cross_signing_key(&user_id);

        // When we attempt to verify it without signing, then it fails
        assert!(!x509_verifier.verify_signed_object(&user_id, &cross_signing_key));

        // But when we sign it
        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        // Then it verifies correctly
        assert!(x509_verifier.verify_signed_object(&user_id, &cross_signing_key));
    }

    #[test]
    fn test_can_verify_cert_containing_email_in_san() {
        // Given a cert containing the email address in the Subject Alternative
        // Name
        let (cert, signing_key) =
            cert_and_key_with_email_in_subject_alternate_name("alice@localhost");

        // When we sign a cross-signing key using it
        let (x509_signer, x509_verifier) = create_signer_and_verifier(cert, signing_key);

        let user_id = user_id!("@alice:localhost").to_owned();
        let mut cross_signing_key = create_cross_signing_key(&user_id);

        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        // Then it verifies correctly.
        assert!(x509_verifier.verify_signed_object(&user_id, &cross_signing_key));
    }

    #[test]
    fn test_can_verify_cert_containing_username_in_san() {
        // Given a cert containing the Matrix user ID in the Subject Alternative
        // Name
        let (cert, signing_key) =
            cert_and_key_with_user_id_in_subject_alternate_name("@alice:localhost");

        // When we sign a cross-signing key using it
        let (x509_signer, x509_verifier) = create_signer_and_verifier(cert, signing_key);

        let user_id = user_id!("@alice:localhost").to_owned();
        let mut cross_signing_key = create_cross_signing_key(&user_id);

        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        // Then it verifies correctly.
        assert!(x509_verifier.verify_signed_object(&user_id, &cross_signing_key));
    }

    #[test]
    fn test_verification_fails_if_dn_email_is_wrong() {
        // Given a cert containing an incorrect email address in the Subject
        // Distinguished Name
        let (cert, signing_key) =
            cert_and_key_with_email_in_subject_distinguished_name("bob@localhost");

        // When we sign a cross-signing key using it
        let (x509_signer, x509_verifier) = create_signer_and_verifier(cert, signing_key);

        let user_id = user_id!("@alice:localhost").to_owned();
        let mut cross_signing_key = create_cross_signing_key(&user_id);

        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        // Then it fails to verify because the supplied email address translates
        // to a different user ID.
        assert!(!x509_verifier.verify_signed_object(&user_id, &cross_signing_key));
    }

    #[test]
    fn test_verification_fails_if_san_email_is_wrong() {
        // Given a cert containing an incorrect email address in the Subject
        // Alternative Name
        let (cert, signing_key) =
            cert_and_key_with_email_in_subject_alternate_name("bob@localhost");

        // When we sign a cross-signing key using it
        let (x509_signer, x509_verifier) = create_signer_and_verifier(cert, signing_key);

        let user_id = user_id!("@alice:localhost").to_owned();
        let mut cross_signing_key = create_cross_signing_key(&user_id);

        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        // Then it fails to verify because the supplied email address translates
        // to a different user ID.
        assert!(!x509_verifier.verify_signed_object(&user_id, &cross_signing_key));
    }

    #[test]
    fn test_verification_fails_if_cert_user_id_is_wrong() {
        // Given a cert containing an incorrect email address in the Subject
        // Alternative Name
        let (cert, signing_key) =
            cert_and_key_with_user_id_in_subject_alternate_name("@bob:localhost");

        // When we sign a cross-signing key using it
        let (x509_signer, x509_verifier) = create_signer_and_verifier(cert, signing_key);

        let user_id = user_id!("@alice:localhost").to_owned();
        let mut cross_signing_key = create_cross_signing_key(&user_id);

        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        // Then it fails to verify because the supplied user ID does not match
        // the signing user.
        assert!(!x509_verifier.verify_signed_object(&user_id, &cross_signing_key));
    }

    #[test]
    fn test_verification_fails_if_cert_user_id_is_missing() {
        // Given a cert with no email or user ID at all
        let (cert, signing_key) = cert_and_key_with_no_user_id();

        // When we sign a cross-signing key using it
        let (x509_signer, x509_verifier) = create_signer_and_verifier(cert, signing_key);

        let user_id = user_id!("@alice:localhost").to_owned();
        let mut cross_signing_key = create_cross_signing_key(&user_id);

        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        // Then it fails to verify because there is no user ID to check against
        // the user's ID.
        assert!(!x509_verifier.verify_signed_object(&user_id, &cross_signing_key));
    }

    fn create_cross_signing_key(user_id: &UserId) -> CrossSigningKey {
        let secret_key = Ed25519SecretKey::new();
        let public_key = secret_key.public_key();
        let keys = SigningKeys::from([(
            DeviceKeyId::from_parts(
                DeviceKeyAlgorithm::Ed25519,
                public_key.to_base64().as_str().into(),
            ),
            public_key.into(),
        )]);

        CrossSigningKey::new(user_id.to_owned(), vec![KeyUsage::Master], keys, Default::default())
    }

    fn create_signer_and_verifier(
        cert: rcgen::Certificate,
        signing_key: rcgen::KeyPair,
    ) -> (X509Signer, X509Verifier) {
        let cert_pem = cert.pem();
        let key_pem = signing_key.serialize_pem();

        let x509_signer = {
            let rust_raw_x509_signer =
                RustRawX509Signer::new_from_pem_data(&cert_pem, &key_pem).unwrap();
            X509Signer::new(Arc::new(rust_raw_x509_signer))
        };

        let x509_verifier = {
            let rust_raw_x509_verifier = RustRawX509Verifier::new_from_pem_data(&cert_pem).unwrap();
            X509Verifier::new(Arc::new(rust_raw_x509_verifier))
        };
        (x509_signer, x509_verifier)
    }
}
