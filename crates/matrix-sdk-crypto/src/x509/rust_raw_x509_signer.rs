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

use rustls::{
    SignatureScheme,
    crypto::aws_lc_rs,
    pki_types::{PrivateKeyDer, pem::PemObject},
    sign::SigningKey,
};
use thiserror::Error;

use crate::{
    SignatureError,
    x509::{
        raw_x509_signature::{RawX509Signature, X509SignatureScheme},
        x509_signer::RawX509Signer,
    },
};

/// A Rust implementation of [`RawX509Signer`]. This does the signing
/// itself (using `rustls`) rather than delegating the work to some external
/// system.
#[derive(Clone)]
pub struct RustRawX509Signer {
    /// The PEM-encoded certificate chain, starting with the device's own
    /// certificate, followed by intermediate certificates.
    certificate_chain: String,

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

impl RustRawX509Signer {
    /// Create a new `RustRawX509Signer` from the supplied PEM data.
    pub fn new_from_pem_data(
        certificate_chain_pem: &str,
        private_key_pem: &str,
    ) -> Result<Self, RustX509SignError> {
        let provider = aws_lc_rs::default_provider();

        let private_key = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())
            .map_err(RustX509SignError::PrivateKeyParseError)?;
        let signing_key: Arc<dyn SigningKey> = provider
            .key_provider
            .load_private_key(private_key)
            .map_err(RustX509SignError::PrivateKeyLoadError)?;

        Ok(Self { certificate_chain: certificate_chain_pem.to_owned(), signing_key })
    }
}

impl RawX509Signer for RustRawX509Signer {
    /// Create a signature for the given message using our private key
    ///
    /// Returns (key ID, signature)
    fn sign(&self, message: &[u8]) -> Result<RawX509Signature, SignatureError> {
        let signer = self
            .signing_key
            .choose_scheme(&[SignatureScheme::RSA_PSS_SHA512])
            .ok_or(SignatureError::UnsupportedAlgorithm)?;

        let signature_bytes =
            signer.sign(message).map_err(|e| SignatureError::X509SigningError(e.to_string()))?;

        Ok(RawX509Signature {
            signature_bytes,
            certificate_chain: self.certificate_chain.clone(),
            signature_scheme: X509SignatureScheme::RsaPssSha512,
        })
    }
}

impl std::fmt::Debug for RustRawX509Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("X509Keys").field(&"<redacted>".to_owned()).finish()
    }
}

#[cfg(test)]
mod tests {
    use crate::x509::{
        RawX509Signer, raw_x509_signature::X509SignatureScheme,
        rust_raw_x509_signer::RustRawX509Signer,
    };

    #[test]
    fn test_can_sign() {
        let x509_sign =
            RustRawX509Signer::new_from_pem_data(TEST_CERT_CHAIN, TEST_CERT_KEY).unwrap();

        let sig = x509_sign.sign(b"hello world").unwrap();

        assert_eq!(sig.certificate_chain, TEST_CERT_CHAIN);
        assert_eq!(sig.signature_scheme, X509SignatureScheme::RsaPssSha512);
        assert_eq!(sig.signature_bytes.len(), 256);
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
