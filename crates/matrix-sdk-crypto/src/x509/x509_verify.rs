use std::{fmt::Debug, sync::Arc};

use ruma::UserId;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tracing::info;

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

        let Some(certificate_email) = get_email_address_from_certificate_subject(&end_cert) else {
            tracing::warn!("Certificate subject does not contain an email address");
            return false;
        };

        let mapped_email = map_user_id_to_email(user_id);
        if certificate_email != mapped_email {
            tracing::warn!(
                "Certificate not valid for this user. Certificate email: {certificate_email}, mapped email: {mapped_email}, user_id: {user_id}",
            );
            return false;
        }

        self.x509_verify.verify(message.as_bytes(), sig)
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

fn get_email_address_from_certificate_subject(certificate: &CertificateDer<'_>) -> Option<String> {
    use x509_parser::prelude::*;
    let (_, parsed_cert) = X509Certificate::from_der(certificate.as_ref()).ok()?;
    let email = parsed_cert.subject.iter_email().next()?;
    email.as_str().ok().map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcgen::{CertificateParams, KeyPair, generate_simple_self_signed};
    use ruma::{DeviceKeyAlgorithm, DeviceKeyId, encryption::KeyUsage, user_id};
    use vodozemac::Ed25519SecretKey;
    use x509_parser::oid_registry::OID_PKCS9_EMAIL_ADDRESS;

    use super::*;
    use crate::{
        types::{CrossSigningKey, SigningKeys},
        x509::{
            X509Signer, rust_raw_x509_signer::RustRawX509Signer,
            rust_raw_x509_verifier::RustRawX509Verifier,
        },
    };

    #[test]
    fn can_extract_email_address_from_a_cert() {
        // Given a certificate containing an email address
        let cert = cert_with_email("myname@company.co.uk");

        // When we extract the email address it contains
        let email = get_email_address_from_certificate_subject(cert.der())
            .expect("Failed to get email address from cert");

        // Then it matches what we put in
        assert_eq!(email, "myname@company.co.uk");
    }

    #[test]
    fn extract_email_address_from_a_cert_that_does_not_contain_one_returns_none() {
        // Given a certificate not containing an email address
        let cert = generate_simple_self_signed(&[]).expect("Failed to generate cert");

        // When we attempt to extract the email address
        let email = get_email_address_from_certificate_subject(cert.cert.der());

        // Then the answer is empty
        assert!(email.is_none());
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
    fn cert_with_email(email: &str) -> rcgen::Certificate {
        let mut cert_params = CertificateParams::default();
        cert_params.distinguished_name.push(
            rcgen::DnType::CustomDnType(OID_PKCS9_EMAIL_ADDRESS.iter().unwrap().collect()),
            email,
        );

        let signing_key = KeyPair::generate().expect("Failed to generate key pair");

        cert_params.self_signed(&signing_key).expect("Failed to generate certificate")
    }

    #[test]
    fn test_can_verify() {
        let x509_signer = {
            let rust_raw_x509_signer =
                RustRawX509Signer::new_from_pem_data(TEST_CERT_CHAIN, TEST_CERT_KEY).unwrap();
            X509Signer::new(Arc::new(rust_raw_x509_signer))
        };

        let user_id = user_id!("@vdh-x509test:sw1v.org").to_owned();

        let mut cross_signing_key = {
            let secret_key = Ed25519SecretKey::new();
            let public_key = secret_key.public_key();
            let keys = SigningKeys::from([(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::Ed25519,
                    public_key.to_base64().as_str().into(),
                ),
                public_key.into(),
            )]);

            CrossSigningKey::new(user_id.clone(), vec![KeyUsage::Master], keys, Default::default())
        };

        let x509_verifier = {
            let rust_raw_x509_verifier =
                RustRawX509Verifier::new_from_pem_data(TEST_CERT_CHAIN).unwrap();
            X509Verifier::new(Arc::new(rust_raw_x509_verifier))
        };

        // We should not be able to verify an unsigned object.
        assert!(!x509_verifier.verify_signed_object(&user_id, &cross_signing_key));

        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        // After the key has been signed, we should be able to verify it.
        assert!(x509_verifier.verify_signed_object(&user_id, &cross_signing_key));
    }

    #[test]
    fn test_verifying_checks_user_id() {
        let x509_signer = {
            let rust_raw_x509_signer =
                RustRawX509Signer::new_from_pem_data(TEST_CERT_CHAIN, TEST_CERT_KEY).unwrap();
            X509Signer::new(Arc::new(rust_raw_x509_signer))
        };

        let user_id = user_id!("@alice:localhost.org").to_owned();

        let mut cross_signing_key = {
            let secret_key = Ed25519SecretKey::new();
            let public_key = secret_key.public_key();
            let keys = SigningKeys::from([(
                DeviceKeyId::from_parts(
                    DeviceKeyAlgorithm::Ed25519,
                    public_key.to_base64().as_str().into(),
                ),
                public_key.into(),
            )]);

            CrossSigningKey::new(user_id.clone(), vec![KeyUsage::Master], keys, Default::default())
        };

        let x509_verifier = {
            let rust_raw_x509_verifier =
                RustRawX509Verifier::new_from_pem_data(TEST_CERT_CHAIN).unwrap();
            X509Verifier::new(Arc::new(rust_raw_x509_verifier))
        };

        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        // Verifying should fail since it was signed using an incorrect user ID
        assert!(!x509_verifier.verify_signed_object(&user_id, &cross_signing_key));
    }

    /// A leaf and intermediate CA cert, generated with openssl
    const TEST_CERT_CHAIN: &str = "
-----BEGIN CERTIFICATE-----
MIIEMDCCAhigAwIBAgIBATANBgkqhkiG9w0BAQsFADBNMQswCQYDVQQGEwJVSzEP
MA0GA1UECAwGTG9uZG9uMRMwEQYDVQQKDAplbGVtZW50LmlvMRgwFgYDVQQDDA9p
bnRlcm1lZGlhdGUtY2EwHhcNMjYwNjExMjI1MjMyWhcNMjcwNjExMjI1MjMyWjAm
MSQwIgYJKoZIhvcNAQkBFhV2ZGgteDUwOXRlc3RAc3cxdi5vcmcwggEiMA0GCSqG
SIb3DQEBAQUAA4IBDwAwggEKAoIBAQCQW4+eDT5UlecWnOeLjzMIcQ/sdRb20uPF
iiRNmzS7kvWIxsTLi2t8pmhQsxqI3CdIWyWfEHy56Np+4HCVCYHoKEc9H/Zm2W2N
63YFTZol7VKVYRAHPHdNh06y6wb1LuzTds5uSpEsvRhqrq14scYCQiFVstdtq5Pc
+F5DpLnGpo1GvzJiF1T5WpNgk9EqcP9Yso4rpwzv1TOsNqX91Tej2tsAZvRbkBms
ILyoFMe3FBwZYGy3nd56Mz/LgxJ4FZEbzHeHQ2i3jpeiz7DESW5VGFUHwg1FdWAG
tTzET+eoVDRC9YzRR+Ka8yQru0kFaLAhAUjvyoMo1Nb8C1IoSTtVAgMBAAGjQjBA
MB0GA1UdDgQWBBSMiQQcHg6Ixffg8lknAK1wY42fjDAfBgNVHSMEGDAWgBR0DWpq
YxJI0M81uFSlqoYvckCA3jANBgkqhkiG9w0BAQsFAAOCAgEArMHX8k+xYTUVAOSR
Vzt8aMMC/LSwN0zQq/+CLX3AuhE94j+2cRM3TF5jpzeRmTObpaK88qVN5CrrK5IC
ddMvMAW+eAUyJc7aXX1tPWD0YRcWlplR0CVm6RY/VIwsVlWvhIX6fYpAJBMoYqr4
tCYcDjA1/FNkuUQnBNnxnp0I3HQuSa1UBCCHZ/00kbVl331XY5bxZrk1OSUWP9TF
pHdW1Fgjqu0EZccjEYK7F1I+weTfkl/uOp6QUl7cmhiLnMsd0QwKUdFStOy1BYX8
tbkMN44diYC9hRUhP9gsud12dBeF4EKl66y43zgUNPAzD3MMmU/xndWIgFs+/PnK
vlbtSMl7wdTr0bh+c3srvY4uuft2Uy0PH9mi0Sffy6LKqg5FUT6kdPtQscZYamb0
TfJ4xjFPII7H6mkvnH/Nm0RWTvsChLeyrPQ8kl328NW0NWlitBZ0k3jiQj0Aqfim
uZwS3f03oqA/254nZU9K6SC5Nvq0L7g71XGK6ZwMVOK3Vw3TV6XLq+nO3FilSrk3
a48QHPLwSRAs7dMIWztnycB/hyMp7daocSieza74AxSwZv6KS6ilEBFv3z9OjTS0
Z5Zy87N5SmP89OfE92IxNQ+tYlnk2THqZSBbSj5hqdHOUVWCzbvHxFEKGSzK3+B5
vDQS6Fr6qeVx8k6TzrvYTf9u61w=
-----END CERTIFICATE-----
-----BEGIN CERTIFICATE-----
MIIFcDCCA1igAwIBAgIBCDANBgkqhkiG9w0BAQsFADBFMQswCQYDVQQGEwJVSzEP
MA0GA1UECAwGTG9uZG9uMRMwEQYDVQQKDAplbGVtZW50LmlvMRAwDgYDVQQDDAd0
ZXN0LWNhMB4XDTI2MDYwNTE1MDIxNloXDTI3MDYwNTE1MDIxNlowTTELMAkGA1UE
BhMCVUsxDzANBgNVBAgMBkxvbmRvbjETMBEGA1UECgwKZWxlbWVudC5pbzEYMBYG
A1UEAwwPaW50ZXJtZWRpYXRlLWNhMIICIjANBgkqhkiG9w0BAQEFAAOCAg8AMIIC
CgKCAgEAt+6qbrTcH+TiHxJl7pJjIajUXvu0nYwnfqDQcaZRY0qlqPuHccBudNfk
FMoD6fchj5OvyQsZLP8UnirLxVgf6GgTLZ2JK4tTpaCFWv45ukHVYeY/r+c0fOuy
zUw4H5LljJ4ikPI3w7vncSQTOCYHpDGVhW8Mh4mAug7ODlL3RZVSsRxuikDhLQoR
OS2pRDwTFll1CYE8+MTkxPcGrnOFQMqB9290af0PnkS64BO0nPUZYqsPELkcPkGU
lc0mTWYunvfG21cU7TnPGRto4nj5l4xZBGpRZSEf3L3yh+4Chi35Rq1hWHlQ8ito
X7n1vNELkHShkQcSmALEmlspGPt6Mhjdb3Xj1N3wNlftXe7g2tFEcnpXu6EwNJYP
mljN1qEA/37xBz9JyZzeahPvOg/hF/+EX3/FcnXLDAYCbFnx/44Ljnr7MHqjQwfH
B7Ndcp22OHC6fwAEjuTPTQlbMmxrxXETKsQwg7aVIPs743R6RJeBQbkUP3hm/7G0
uwoB2kphdjvGHYJtDguNXHuiXz/Pn6F+D6m3j/SDF4uAfnzxKDDVURPlQg2Wr2od
ez0aklHgJ9ZUAfpk3C6f1VGUmxpjVhp6nPsQ5ZgGFHNOjWZuV2+cwxahpYG6KzVD
mO/brmSc76ibzRwapIOLSjb+plbp8VGrBElIhvqRFyZmTOcGdLkCAwEAAaNjMGEw
HQYDVR0OBBYEFHQNampjEkjQzzW4VKWqhi9yQIDeMB8GA1UdIwQYMBaAFNhekZoX
8MNbE9t1Qn0hN5rfPpYRMA8GA1UdEwEB/wQFMAMBAf8wDgYDVR0PAQH/BAQDAgGG
MA0GCSqGSIb3DQEBCwUAA4ICAQA1X27xupoF0brABykMbCE4Q1QQ2u3oLUOoX0vP
EVBjGC7tlIe6O1+s8lCSSM6pAly61kMl2rIXE7GzPMLVNjg67etXfNbb8KXPqiKi
bPPLQ6Pk5/RJd3D8DUO4gKgEKtkBZK30JzKVsl5eLgxT1GVnDXgV9jNLqXuNgreZ
Ba4BYjdtR2pMbVlW0ZEda7m4N5CDh8rfJQqnZeudhzqjecXV5bxxvKotvOEgsYml
WZ4yxAwuJhlb+I/Rmc+hocsRBx8v8LIIQh/biUMaZnoCqj8d9qkpFmf9nWhXQRbJ
Sqz2ad08ZXxMY+vXHQ3mR1H4cYm5jOWuyXWSULo/WT+OrGPFrH1/E3IjhvVaal6m
c4IJQZCI7iHhyeHLxjaVmctgwV2yMUZnCuBXrv0yHkkDyr1D6OBaHF/0dYSd/rym
ZDo1S4QcKmoJJRP0nDdq7zMoLneu7Ytmn3PZ8qiAUnh5vlaQCg3SHkdQ0berp7yU
+0tFAPc7c4+yad+YBxLi+Zqplqw9Ra810Nn+coBH54+DOytWQQdK4kx0pXc+2MYR
sykQE11ItydeFcAu2Rtp6dw5G9DNOIQJWYeGOECx6phCZYrZVEPZoeF0dnsEcdP8
F5aVeO+O1gffTCxzhLTqMXNvm86oP6QC71gC1w6eM4uuGubc56Dm4hxVywwRUGgJ
tsmVjQ==
-----END CERTIFICATE-----
";

    /// Private key for the leaf certificate above
    const TEST_CERT_KEY: &str = "
-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCQW4+eDT5UlecW
nOeLjzMIcQ/sdRb20uPFiiRNmzS7kvWIxsTLi2t8pmhQsxqI3CdIWyWfEHy56Np+
4HCVCYHoKEc9H/Zm2W2N63YFTZol7VKVYRAHPHdNh06y6wb1LuzTds5uSpEsvRhq
rq14scYCQiFVstdtq5Pc+F5DpLnGpo1GvzJiF1T5WpNgk9EqcP9Yso4rpwzv1TOs
NqX91Tej2tsAZvRbkBmsILyoFMe3FBwZYGy3nd56Mz/LgxJ4FZEbzHeHQ2i3jpei
z7DESW5VGFUHwg1FdWAGtTzET+eoVDRC9YzRR+Ka8yQru0kFaLAhAUjvyoMo1Nb8
C1IoSTtVAgMBAAECggEAEOKg2SYn6wlwsxitzcl1ePCOogQtKDhRO6c1qV007Rba
wQGs/bkEXNtzGrNkcGs97gz5SNKLIEzQF+SlTo2C5Dan5IqzTeLzWVUYJDUoSXTp
wr7Meugz9T3VMwDiOrYfLfn42fY/ZmoE69+cO2Ch7lwxXX6Si8m0vTVRA10GfOmL
2xcaXTN5EcDT4aA9IuEEzs3bGd1j4+WufDtPVV5zZXQLPXzxMBqegphnqW36ACs4
14a6HaWOfy+ggy52xxx92TeHO5Sf6MX7o2BIIEp2UXGEl8Nur7UTifZpiiqfGbxj
z2AD69Q8+Oh6qiM49JNKUJ1FKpeHrUtI/qdZpKAvuQKBgQDI+7wLvMAfQ+xRUv4u
EHhWh5uK5PTf9YojfPqGsLls5xzHlwIa2CYNbWFk7/grFL2lT0uv3beoVcxSQC8+
ITMxrNl2OnqSwIiLF3rAIEQn/XfeiU/cdq1wakLEBdeNxCY3G4p7vk85s+KAkoH/
ebs2ChuXnuTQGQf32S4GGdQU+QKBgQC3365jChmHK0LW8pJyjJIJrWjvQ9dLXb30
j6dnCWPd6M0dK5+NfumOPMvw15cu6tkjs7IMwxFcnSN84h1BKe79PGSTaLl1JaOC
wo24ZGqTZZEELkehSWqc6isBce5aVInXUzBH9MSvxvLdXlW+zaItH/PvSRwO2sUS
kSY0sF0cPQKBgQCE/odt4OXlCoZLPjbydnWbFLspit47gPh7CU2iaTkaRki2DkgX
SWbMxc+IAn9eyqe/xxwXcQkB/FxrJQvd+gwtV+rCoGnRyFPSbqQMlI1lRQXYHVba
VTHpzHcHzbHYnq6HEtNtlP5J+a3tVIVvb7chSEj/6OYSii3KpU0ePmMnyQKBgGpp
oVrf9XYsqzoKmIaCo+HF4fzWnjqXvd9TY+ZVoN5EZLCFFomk8TXIKZ7wpiYY9CGd
VWXdXqbiqi8UDSoxQoZ79Rj6epo5di+uuKYGN0emeA6bWgkVnAXD36+uZ+sPEdbz
5fU+yrWPxe4nMiiCiWDkJSBOh1ZxdawRJLNJfhlhAoGBAMF2/htOA9Zucf23I/7X
xvt0tTxmh11PlmWJqAdP4w8Pi0xM6LgXpsnU8tFhK/ouHsgshcrDfZpugE7j6ZLv
xJoRUZyn77BUtdyTi/hbMsK5v9GzujTmK2hXYH+mamYRStsIZWFB1haAGT7W6njW
oJQP+ghjUYgZHGzfyheUm0L6
-----END PRIVATE KEY-----
";
}
