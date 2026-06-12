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
// limitations under the License

use std::sync::Arc;

use rustls::{
    RootCertStore,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, UnixTime, pem::PemObject},
    server::{VerifierBuilderError, WebPkiClientVerifier, danger::ClientCertVerifier},
};
use thiserror::Error;
use vodozemac::base64_decode;
use webpki::EndEntityCert;

use crate::{types::X509Signature, x509::x509_verify::RawX509Verifier};

#[derive(Error, Debug)]
pub enum RustX509VerifyError {
    /// There was an error parsing the certificate
    #[error("failed to parse certificate")]
    ParseError(rustls::pki_types::pem::Error),

    /// There was an error building the verifier
    #[error("failed to build verifier {0}")]
    VerifierBuilderError(VerifierBuilderError),

    /// There was an error adding the certificate to the root store.
    #[error("failed to add certificate to root store {0}")]
    StoreError(rustls::Error),
}

#[derive(Debug, Clone)]
pub struct RustRawX509Verifier {
    verifier: Arc<dyn ClientCertVerifier>,
}

/// A Rust implementation of [`RawX509Verifier`]. This does the verification
/// itself (using `rustls`) rather than delegating the work to some external
/// system.
impl RustRawX509Verifier {
    /// Create a new `RustRawX509Verifier` from the supplied CA certificates
    /// PEM.
    pub fn new_from_pem_data(ca_certs_pem: &str) -> Result<Self, RustX509VerifyError> {
        let mut root_store = RootCertStore::empty();
        for result in CertificateDer::pem_slice_iter(ca_certs_pem.as_bytes()) {
            root_store
                .add(result.map_err(RustX509VerifyError::ParseError)?)
                .map_err(RustX509VerifyError::StoreError)?;
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(root_store))
            //.with_crls(...)
            .build()
            .map_err(RustX509VerifyError::VerifierBuilderError)?;

        Ok(Self { verifier })
    }
}

impl RawX509Verifier for RustRawX509Verifier {
    fn verify(&self, message: &[u8], sig: &X509Signature) -> bool {
        let mut cert_iter = CertificateDer::pem_slice_iter(sig.certificate_chain.as_bytes());
        let Some(Ok(end_cert)) = cert_iter.next() else {
            tracing::warn!("Missing or invalid first certificate");
            return false;
        };
        let Ok(intermediate_certs): Result<Vec<_>, _> = cert_iter.collect() else {
            tracing::warn!("Invalid certificate in the chain");
            return false;
        };

        // Verify the certificate chain is issued by one of our trusted roots
        if self
            .verifier
            .verify_client_cert(&end_cert, intermediate_certs.as_ref(), UnixTime::now())
            .is_err()
        {
            // Not an error: could happen if some certificate has expired.
            return false;
        }

        // Verify this is a signature of the master key
        let Some(provider) = CryptoProvider::get_default() else {
            tracing::error!("Unable to get default rustls crypto provider");
            return false;
        };

        let Some(alg) = provider
            .signature_verification_algorithms
            .mapping
            .iter()
            // Filter for entries with the right signature scheme
            .filter(|item| item.0 == sig.signature_scheme)
            // Filter for entries with a non-empty list of algorithm implementations, and get the
            // first such implementation
            .filter_map(|item| item.1.first().map(|alg| *alg))
            .next()
        else {
            tracing::warn!("Signature scheme {:?} not supported", sig.signature_scheme);
            return false;
        };

        let Ok(cert) = EndEntityCert::try_from(&end_cert) else {
            tracing::warn!("Unable to parse certificate");
            return false;
        };

        // TODO: AJB: make it harder to forget to base64 encode/decode this?
        let Ok(signature_bin) = base64_decode(&sig.signature) else {
            tracing::warn!("Failed to base64-decode the signaturea");
            return false;
        };
        let result = cert.verify_signature(alg, message, &signature_bin);

        if let Err(e) = result {
            tracing::warn!("Signature verification failed: {e}");
            return false;
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use crate::x509::{
        RawX509Signer, RawX509Verifier, rust_raw_x509_signer::RustRawX509Signer,
        rust_raw_x509_verifier::RustRawX509Verifier,
    };

    #[test]
    fn can_verify() {
        let x509_sign =
            RustRawX509Signer::new_from_pem_data(TEST_CERT_CHAIN, TEST_CERT_KEY).unwrap();

        // After we sign a text
        let (_key_id, sig) = x509_sign.sign(b"hello world").unwrap();

        let x509_verify = RustRawX509Verifier::new_from_pem_data(TEST_CERT_CHAIN).unwrap();

        // checking the signature on the same string should succeed
        assert!(x509_verify.verify(b"hello world", &sig));

        // checking the signature on a different string should fail
        assert!(!x509_verify.verify(b"Hello World", &sig));

        // checking the signature with an unknown algorithm should fail
        let sig_with_unknown_alg = {
            let mut sig = sig.clone();
            sig.signature_scheme = 0.into();
            sig
        };
        assert!(!x509_verify.verify(b"hello world", &sig_with_unknown_alg));

        // checking the signature with a bad certificate should fail
        let sig_with_bad_certificate_chain = {
            let mut sig = sig.clone();
            sig.certificate_chain = "".to_owned();
            sig
        };
        assert!(!x509_verify.verify(b"hello world", &sig_with_bad_certificate_chain));
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
