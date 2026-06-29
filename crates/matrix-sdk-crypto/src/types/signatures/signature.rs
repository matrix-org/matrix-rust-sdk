/*
Copyright 2022-2026 The Matrix.org Foundation C.I.C.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

    http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/
use as_variant::as_variant;
use ruma::DeviceKeyAlgorithm;
use vodozemac::Ed25519Signature;

use crate::types::InvalidSignature;
#[cfg(feature = "experimental-x509-identity-verification")]
use crate::types::{X509_SIGNATURE_ALGORITHM, X509Signature};

/// Represents a potentially decoded signature (but *not* a validated one).
///
/// There are two important cases here:
///
/// 1. If the claimed algorithm is supported *and* the payload has an expected
///    format, the signature will be represent by the enum variant corresponding
///    to that algorithm. For example, decodable Ed25519 signatures are
///    represented as `Ed25519(...)`.
/// 2. If the claimed algorithm is unsupported, the signature is represented as
///    `Other(...)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Signature {
    /// A Ed25519 digital signature.
    Ed25519(Ed25519Signature),
    /// An X.509 digital signature.
    #[cfg(feature = "experimental-x509-identity-verification")]
    X509(X509Signature),
    /// A digital signature in an unsupported algorithm. The raw signature bytes
    /// are represented as a base64-encoded string.
    Other(String),
}

impl Signature {
    /// Get the Ed25519 signature, if this is one.
    pub fn ed25519(&self) -> Option<Ed25519Signature> {
        as_variant!(self, Self::Ed25519).copied()
    }

    /// Decode a signature from the Base64-encoded format used in Matrix
    /// `signatures` maps.
    pub fn from_base64(algorithm: DeviceKeyAlgorithm, s: String) -> Result<Self, InvalidSignature> {
        match algorithm {
            DeviceKeyAlgorithm::Ed25519 => Ed25519Signature::from_base64(&s)
                .map(|s| s.into())
                .map_err(|_| InvalidSignature { source: s }),

            #[cfg(feature = "experimental-x509-identity-verification")]
            DeviceKeyAlgorithm::_Custom(_) if algorithm == X509_SIGNATURE_ALGORITHM.into() => {
                X509Signature::from_str(&s)
                    .map(|s| s.into())
                    .map_err(|_| InvalidSignature { source: s })
            }

            _ => Ok(Signature::Other(s)),
        }
    }

    /// Convert the signature to a base64 encoded string.
    pub fn to_base64(&self) -> String {
        match self {
            Signature::Ed25519(s) => s.to_base64(),
            #[cfg(feature = "experimental-x509-identity-verification")]
            Signature::X509(s) => s.to_string(),
            Signature::Other(s) => s.to_owned(),
        }
    }
}

impl From<Ed25519Signature> for Signature {
    fn from(signature: Ed25519Signature) -> Self {
        Self::Ed25519(signature)
    }
}

#[cfg(feature = "experimental-x509-identity-verification")]
impl From<X509Signature> for Signature {
    fn from(signature: X509Signature) -> Self {
        Self::X509(signature)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn roundtrip_ed25519_signature() {
        const BASE64_SIG: &str = "AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQ";

        let parsed = Signature::from_base64(DeviceKeyAlgorithm::Ed25519, BASE64_SIG.to_owned())
            .expect("Failed to parse");

        let ed25519 = parsed.ed25519().expect("Not parsed as Ed25519");
        assert_eq!(ed25519.to_base64(), BASE64_SIG);

        let encoded = parsed.to_base64();
        assert_eq!(encoded, BASE64_SIG)
    }

    #[test]
    fn parse_invalid_ed25519_signature() {
        const BASE64_SIG: &str = "XXXX";

        let parsed = Signature::from_base64(DeviceKeyAlgorithm::Ed25519, BASE64_SIG.to_owned())
            .expect_err("Expected an invalid signature");
        assert_eq!(parsed.source, BASE64_SIG);
    }

    #[test]
    fn roundtrip_other_signature() {
        const TEXT: &str = "abcd";

        let parsed = Signature::from_base64(DeviceKeyAlgorithm::from("foo"), TEXT.to_owned())
            .expect("Failed to parse");

        let other = as_variant!(&parsed, Signature::Other).expect("Not parsed as Other");
        assert_eq!(other, TEXT);

        let encoded = parsed.to_base64();
        assert_eq!(encoded, TEXT)
    }
}
