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
        X509SignatureSigningError,
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

/// An enum of possible errors that can occur while instantiating
/// a [`RustRawX509Signer`].
#[derive(Error, Debug)]
pub enum RustX509SignError {
    /// There was an error parsing the certificate chain.
    #[error("failed to parse certificate chain {0}")]
    CertificateParse(rustls::pki_types::pem::Error),

    /// No certificates were found.
    #[error("no certificates found in chain")]
    CertificateNotFound,

    /// There was an error parsing the private key.
    #[error("failed to parse private key {0}")]
    PrivateKeyParse(rustls::pki_types::pem::Error),

    /// There was an error loading the private key.
    #[error("failed to load private key {0}")]
    PrivateKeyLoad(rustls::Error),
}

impl RustRawX509Signer {
    /// Create a new `RustRawX509Signer` from the supplied PEM data.
    pub fn new_from_pem_data(
        certificate_chain_pem: &str,
        private_key_pem: &str,
    ) -> Result<Self, RustX509SignError> {
        let provider = aws_lc_rs::default_provider();

        let private_key = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())
            .map_err(RustX509SignError::PrivateKeyParse)?;
        let signing_key: Arc<dyn SigningKey> = provider
            .key_provider
            .load_private_key(private_key)
            .map_err(RustX509SignError::PrivateKeyLoad)?;

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

        let signature_bytes = signer.sign(message).map_err(|e| {
            SignatureError::X509SigningError(X509SignatureSigningError::Custom(e.into()))
        })?;

        Ok(RawX509Signature {
            signature_bytes,
            certificate_chain: self.certificate_chain.clone(),
            signature_scheme: X509SignatureScheme::RsaPssSha512,
        })
    }
}

impl std::fmt::Debug for RustRawX509Signer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RustRawX509Signer")
            .field("certificate_chain", &self.certificate_chain)
            .field("signing_key", &"<redacted>".to_owned())
            .finish()
    }
}
