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

use std::sync::Arc;

use ruma::{DeviceKeyId, OwnedDeviceKeyId};
use rustls::{
    SignatureScheme,
    crypto::aws_lc_rs,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    sign::SigningKey,
};
use thiserror::Error;
use vodozemac::base64_encode;

use crate::{SignatureError, types::X509Signature, x509::x509_signer::X509Sign};

/// A Rust implementation of [`X509Sign`]. This does the verification itself
/// (using `rustls`) rather than delegating the work to some external system.
#[derive(Clone)]
pub struct RustX509Sign {
    /// The PEM-encoded certificate chain, starting with the device's own
    /// certificate, followed by intermediate certificates.
    certificate_chain: String,

    /// The key ID for signatures we generate.
    device_id: OwnedDeviceKeyId,

    /// The private signing key for this device.
    signing_key: Arc<dyn SigningKey>,
}

#[derive(Error, Debug)]
pub enum RustX509SignError {
    /// There was an error parsing the certificate chain.
    #[error("failed to parse certificate chain {0}")]
    CertificateParseError(rustls::pki_types::pem::Error),

    /// No certificates were found.
    #[error("no certificates found in chain")]
    CertificateNotFoundError,

    /// There was an error parsing the private key.
    #[error("failed to parse private key {0}")]
    PrivateKeyParseError(rustls::pki_types::pem::Error),

    /// There was an error loading the private key.
    #[error("failed to load private key {0}")]
    PrivateKeyLoadError(rustls::Error),
}

impl RustX509Sign {
    /// Create a new `RustX509Sign` from the supplied PEM data.
    pub fn new_from_pem_data(
        certificate_chain_pem: &str,
        private_key_pem: &str,
    ) -> Result<Self, RustX509SignError> {
        let provider = aws_lc_rs::default_provider();

        let cert_iter = CertificateDer::pem_slice_iter(certificate_chain_pem.as_bytes());
        let last_cert = cert_iter
            .last()
            .ok_or(RustX509SignError::CertificateNotFoundError)?
            .map_err(|e| RustX509SignError::CertificateParseError(e))?;
        let last_cert_aki =
            get_authority_key_identifier(&last_cert).expect("no AKI found in last cert");
        let device_id =
            DeviceKeyId::from_parts("io.element.x509".into(), last_cert_aki.as_str().into());

        let private_key = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())
            .map_err(|e| RustX509SignError::PrivateKeyParseError(e))?;
        let signing_key: Arc<dyn SigningKey> = provider
            .key_provider
            .load_private_key(private_key)
            .map_err(|e| RustX509SignError::PrivateKeyLoadError(e))?;

        Ok(Self { certificate_chain: certificate_chain_pem.to_owned(), device_id, signing_key })
    }
}

impl X509Sign for RustX509Sign {
    /// Create a signature for the given message using our private key
    ///
    /// Returns (key ID, signature)
    fn sign(&self, message: &[u8]) -> Result<(OwnedDeviceKeyId, X509Signature), SignatureError> {
        let signature_scheme = SignatureScheme::RSA_PSS_SHA512;
        let signer = self
            .signing_key
            .choose_scheme(&[signature_scheme])
            .ok_or(SignatureError::UnsupportedAlgorithm)?;

        let signature = signer.sign(message).map_err(|e| SignatureError::X509SigningError(e))?;
        Ok((
            self.device_id.clone(),
            X509Signature {
                certificate_chain: self.certificate_chain.clone(),
                signature_scheme,
                signature: base64_encode(signature),
            },
        ))
    }
}

impl std::fmt::Debug for RustX509Sign {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("X509Keys").field(&"<redacted>".to_owned()).finish()
    }
}

/// Extract the X509v3 Authority Key Identifier from a certificate, as a
/// base64-encoded string.
///
/// TODO: add some error handling, rather than just returning None. All the
/// certs we look at should have the AKI extension, so we should return an error
/// if it isn't found.
fn get_authority_key_identifier(cert: &CertificateDer<'_>) -> Option<String> {
    use x509_parser::prelude::*;

    let (_, parsed_cert) = X509Certificate::from_der(cert.as_ref()).ok()?;

    parsed_cert
        .get_extension_unique(&oid_registry::OID_X509_EXT_AUTHORITY_KEY_IDENTIFIER)
        .ok()?
        .and_then(|ext| match ext.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(x) => x.key_identifier.as_ref(),
            _ => None,
        })
        .map(|id| base64_encode(id.0))
}

#[cfg(test)]
mod tests {
    use rustls::pki_types::{CertificateDer, pem::PemObject};

    use crate::x509::{
        X509Sign,
        rust_x509_sign::{RustX509Sign, get_authority_key_identifier},
    };

    #[test]
    fn test_get_authority_key_identifier() {
        // Given the DER-encoded intermediate certificate
        let cert_iter = CertificateDer::pem_slice_iter(TEST_CERT_CHAIN.as_bytes());
        let last_cert = cert_iter
            .last()
            .expect("unable to parse certificate chain")
            .expect("no certificates found in chain");

        // When we extract the AKI from it
        let aki = get_authority_key_identifier(&last_cert);

        // We should get the base64-encoding of
        // D8:5E:91:9A:17:F0:C3:5B:13:DB:75:42:7D:21:37:9A:DF:3E:96:11
        assert_eq!(aki.expect("no AKI found"), "2F6Rmhfww1sT23VCfSE3mt8+lhE");
    }

    #[test]
    fn can_sign() {
        let x509_sign = RustX509Sign::new_from_pem_data(TEST_CERT_CHAIN, TEST_CERT_KEY).unwrap();

        let (key_id, sig) = x509_sign.sign(b"hello world").unwrap();

        assert_eq!(key_id.as_str(), "io.element.x509:2F6Rmhfww1sT23VCfSE3mt8+lhE");
        assert_eq!(sig.certificate_chain, TEST_CERT_CHAIN);
        assert_eq!(u16::from(sig.signature_scheme), 2054); // SignatureScheme::RSA_PSS_SHA512
        assert_eq!(sig.signature.len(), 342);
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
