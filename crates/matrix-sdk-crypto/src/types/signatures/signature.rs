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
use vodozemac::Ed25519Signature;

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
    /// A digital signature in an unsupported algorithm. The raw signature bytes
    /// are represented as a base64-encoded string.
    Other(String),
}

impl Signature {
    /// Get the Ed25519 signature, if this is one.
    pub fn ed25519(&self) -> Option<Ed25519Signature> {
        as_variant!(self, Self::Ed25519).copied()
    }

    /// Convert the signature to a base64 encoded string.
    pub fn to_base64(&self) -> String {
        match self {
            Signature::Ed25519(s) => s.to_base64(),
            Signature::Other(s) => s.to_owned(),
        }
    }
}

impl From<Ed25519Signature> for Signature {
    fn from(signature: Ed25519Signature) -> Self {
        Self::Ed25519(signature)
    }
}
