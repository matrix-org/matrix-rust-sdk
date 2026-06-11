use std::sync::Arc;

use ruma::{OwnedDeviceKeyId, UserId, canonical_json::to_canonical_value};

use crate::{
    SignatureError,
    olm::utility::to_signable_json,
    types::{CrossSigningKey, X509Signature},
};

/// Hold one of these if you want to sign cross-signing keys, and call
/// [`Self::sign_cross_signing_key`] to do it.
///
/// Internally, this holds an implementation of [`RawX509Signer`] that does the
/// real work of signing things. This struct provides a convenient wrapper that
/// e.g. converts a cross-signing key to signable canonical JSON.
#[derive(Debug, Clone)]
pub struct X509Signer {
    x509_sign: Arc<dyn RawX509Signer>,
}

impl X509Signer {
    /// Create a new `X509Signer` that wraps the supplied [`RawX509Signer`].
    pub fn new(x509_sign: Arc<dyn RawX509Signer>) -> Self {
        Self { x509_sign }
    }

    /// Add a signature to the given cross-signing key using our private X.509
    /// key.
    pub fn sign_cross_signing_key(
        &self,
        signing_user_id: &UserId,
        cross_signing_key: &mut CrossSigningKey,
    ) -> Result<(), SignatureError> {
        let json = to_signable_json(to_canonical_value(&cross_signing_key)?)?;

        let (device_key_id, signature) = self.x509_sign.sign(json.as_bytes())?;

        cross_signing_key.signatures.add_signature(
            signing_user_id.to_owned(),
            device_key_id,
            signature,
        );

        Ok(())
    }
}

/// A low-level interface for signing messages with a private key. We have Rust
/// and platform-specific implementations.
pub trait RawX509Signer: std::fmt::Debug + Send + Sync {
    /// Create a signature for the given message using our private key
    ///
    /// Returns (key ID, signature)
    fn sign(&self, message: &[u8]) -> Result<(OwnedDeviceKeyId, X509Signature), SignatureError>;
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use ruma::{DeviceKeyAlgorithm, DeviceKeyId, encryption::KeyUsage, user_id};
    use vodozemac::Ed25519SecretKey;

    use crate::{
        types::{CrossSigningKey, Signature, SigningKeys},
        x509::{X509Signer, rust_raw_x509_signer::RustRawX509Signer},
    };

    #[test]
    fn can_sign() {
        let x509_signer = {
            let rust_raw_x509_signer =
                RustRawX509Signer::new_from_pem_data(TEST_CERT_CHAIN, TEST_CERT_KEY).unwrap();
            X509Signer::new(Arc::new(rust_raw_x509_signer))
        };

        let user_id = user_id!("@alice:localhost").to_owned();

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

        x509_signer.sign_cross_signing_key(&user_id, &mut cross_signing_key).unwrap();

        let self_sigs = cross_signing_key.signatures.get(&user_id).unwrap();

        // The cross-signing key should now have an X.509 signature from the
        // given key.
        assert_matches!(
            self_sigs
                .get(&DeviceKeyId::from_parts(
                    "io.element.x509".into(),
                    "2F6Rmhfww1sT23VCfSE3mt8+lhE".into()
                ))
                .unwrap(),
            Ok(Signature::X509(_))
        );
    }

    /// A leaf and intermediate CA cert, generated with openssl
    const TEST_CERT_CHAIN: &str = "
-----BEGIN CERTIFICATE-----
MIIEMTCCAhmgAwIBAgIBADANBgkqhkiG9w0BAQsFADBNMQswCQYDVQQGEwJVSzEP
MA0GA1UECAwGTG9uZG9uMRMwEQYDVQQKDAplbGVtZW50LmlvMRgwFgYDVQQDDA9p
bnRlcm1lZGlhdGUtY2EwHhcNMjYwNjA1MTUwNTI1WhcNMjcwNjA1MTUwNTI1WjAn
MSUwIwYJKoZIhvcNAQkBFhZAdmRoLXg1MDl0ZXN0OnN3MXYub3JnMIIBIjANBgkq
hkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAkFuPng0+VJXnFpzni48zCHEP7HUW9tLj
xYokTZs0u5L1iMbEy4trfKZoULMaiNwnSFslnxB8uejafuBwlQmB6ChHPR/2Ztlt
jet2BU2aJe1SlWEQBzx3TYdOsusG9S7s03bObkqRLL0Yaq6teLHGAkIhVbLXbauT
3PheQ6S5xqaNRr8yYhdU+VqTYJPRKnD/WLKOK6cM79UzrDal/dU3o9rbAGb0W5AZ
rCC8qBTHtxQcGWBst53eejM/y4MSeBWRG8x3h0Not46Xos+wxEluVRhVB8INRXVg
BrU8xE/nqFQ0QvWM0UfimvMkK7tJBWiwIQFI78qDKNTW/AtSKEk7VQIDAQABo0Iw
QDAdBgNVHQ4EFgQUjIkEHB4OiMX34PJZJwCtcGONn4wwHwYDVR0jBBgwFoAUdA1q
amMSSNDPNbhUpaqGL3JAgN4wDQYJKoZIhvcNAQELBQADggIBAHVck9j2uVf19bTh
fMs4s61/PWzzo8oE7EjTVKMwuA8dBs/e0hR+AL5iFbSBBEfWIWGd8859+xkddlIS
IL6Q+Uh8pYLh0NCdypIJfULD0bvU+OelYzLSnjznPr4fUsqBeNxLLpXI+Ft43C56
r5weAR0OwUyIYiXwleU3brUVQuSEThllh4uhXxqgLng/U/BPOdmAsQwjjrc6ZGP1
S4aNNcfTeSEIBwDvfuPch4thI1oMUDnnoAZiYJCIpi4Kn14lMZb2pakALLJd2LZ6
QD9/uKnF8bPiO5Jvi7hq/RUZDBVgxU1gCbJXiHq8T83/tlnIOud8xOXdxPSCRgrj
AFqCdgZ/AGKD7hhLr6NBUghfMrsVzRV1tOwchxNdECBLCvSg9PsE9HlVV+2CjMy2
XQ0Qxfr/wIDAzbqxywc8u4C6ew+VacwoSneKhHycR8CHBTPJpVGcUTtQvBWQbUiU
xbMuNt86a5L7eeVwKn1hvY9HfaFhOe0FM3/G7WMo7HjBdFuKG41TYAFF26tw2Pnq
CfuKxOLgLYNdbJL1Vs06QfWl39FtKN9GPTsx3Lfa1zrdXIljVdJ0U88AEMmgQsOV
oAp5r/OXnVT0fm+gW6mvAqWOdQb7EO6vnyd0mGWQi5RIfPeaIaUUHezBJ5P6/OnS
XizEmWnNxHOTdDHfql91PPJCDuQZ
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
