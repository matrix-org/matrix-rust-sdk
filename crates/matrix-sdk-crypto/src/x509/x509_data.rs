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

use std::{fmt::Debug, sync::Arc};

use crate::x509::{
    rust_x509_sign::RustX509Sign, rust_x509_verify::RustX509Verify, x509_signer::X509Signer,
    x509_verify::X509Verifier,
};

/// The information needed to sign keys and verify signatures using
/// externally-provided X.509 keys and trust roots.
#[derive(Debug, Clone)]
pub struct X509Data {
    /// Private key for this device
    pub x509_signer: Option<X509Signer>,

    /// Trusted root certificates
    pub x509_verifier: Option<X509Verifier>,
}

impl X509Data {
    /// Create an X509Data from the supplied PEM-format strings.
    pub fn from_pem_data(ca_certs_pem: &str, private_key_pem: &str, cert_chain_pem: &str) -> Self {
        // TODO: it would be sensible to validate that the private key matches the
        //   certificate chain here, to catch configuration errors early.

        X509Data {
            x509_signer: Some(X509Signer::new(Arc::new(RustX509Sign::new_from_pem_data(
                cert_chain_pem,
                private_key_pem,
            )))),
            x509_verifier: Some(X509Verifier::new(Arc::new(RustX509Verify::new_from_pem_data(
                ca_certs_pem,
            )))),
        }
    }
}
