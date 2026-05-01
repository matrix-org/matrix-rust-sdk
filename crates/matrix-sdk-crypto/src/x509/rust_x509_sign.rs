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
use vodozemac::base64_encode;

use crate::{SignatureError, types::X509Signature, x509::x509_signer::X509Sign};

#[derive(Clone)]
pub struct RustX509Sign {
    /// The PEM-encoded certificate chain, starting with the device's own
    /// certificate, followed by intermediate certificates.
    certificate_chain: String,

    /// The private signing key for this device.
    signing_key: Arc<dyn SigningKey>,
}

impl RustX509Sign {
    pub(crate) fn new_from_pem_data(certificate_chain_pem: &str, private_key_pem: &str) -> Self {
        let provider = aws_lc_rs::default_provider();
        let private_key = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())
            .expect("unable to parse private key");
        let signing_key: Arc<dyn SigningKey> = provider
            .key_provider
            .load_private_key(private_key)
            .expect("unable to load private key");

        Self { certificate_chain: certificate_chain_pem.to_owned(), signing_key }
    }
}

impl X509Sign for RustX509Sign {
    /// Create a signature for the given message using our private key
    ///
    /// Returns (key ID, signature)
    fn sign(&self, message: &[u8]) -> Result<(String, X509Signature), SignatureError> {
        // TODO RAV: error handling

        let signature_scheme = SignatureScheme::RSA_PSS_SHA512;
        let signer = self
            .signing_key
            .choose_scheme(&[signature_scheme])
            .expect("unable to choose signature scheme");

        let signature = signer.sign(message).expect("unable to sign");
        // TODO RAV: key id
        Ok((
            "x509:todo_key_id".to_owned(),
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
