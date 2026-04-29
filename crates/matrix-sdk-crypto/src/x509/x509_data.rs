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

use crate::x509::{X509Keys, X509TrustRoot};

#[derive(Debug, Clone)]
pub struct X509Data {
    /// Private key for this device
    pub x509_key: X509Keys,

    /// Trusted root certificates
    pub x509_trust_root: X509TrustRoot,
}

impl X509Data {
    pub fn from_pem_data(ca_certs_pem: &str, private_key_pem: &str, cert_chain_pem: &str) -> Self {
        // TODO: it would be sensible to validate that the private key matches the
        //   certificate chain here, to catch configuration errors early.

        X509Data {
            x509_key: X509Keys::new_from_pem_data(cert_chain_pem, private_key_pem),
            x509_trust_root: X509TrustRoot::new_from_pem_data(ca_certs_pem),
        }
    }
}
