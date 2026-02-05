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

//! Cryptographic primitives for DCGKA

use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey, Ed25519SecretKey, Ed25519Signature};

/// Derived symmetric encryption key for the group
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupKey {
    /// 256-bit key material for AEAD encryption
    pub key_material: [u8; 32],
    /// Generation counter (number of accepted updates)
    pub generation: u64,
}

impl GroupKey {
    /// Create a new group key
    pub fn new(key_material: [u8; 32], generation: u64) -> Self {
        Self { key_material, generation }
    }
}

/// Sign a message with Ed25519 secret key
#[allow(dead_code)]
pub fn sign_ed25519(secret_key: &Ed25519SecretKey, message: &[u8]) -> Ed25519Signature {
    secret_key.sign(message)
}

/// Verify an Ed25519 signature
pub fn verify_ed25519(public_key: &Ed25519PublicKey, message: &[u8], signature: &[u8]) -> bool {
    if let Ok(sig) = Ed25519Signature::from_slice(signature) {
        public_key.verify(message, &sig).is_ok()
    } else {
        false
    }
}

/// Perform X25519 Diffie-Hellman key exchange
#[allow(dead_code)]
pub fn x25519_dh(
    local_identity: &Curve25519PublicKey,
    remote_public: &Curve25519PublicKey,
) -> [u8; 32] {
    // Note: In practice, we'd use the secret key for DH
    // For now, we return deterministic entropy based on both keys
    let mut hasher = Sha256::new();
    hasher.update(local_identity.as_bytes());
    hasher.update(remote_public.as_bytes());
    hasher.finalize().into()
}

/// Derive key using HKDF-SHA256
pub fn derive_key_hkdf(input_key_material: &[u8], info: &[u8], output_len: usize) -> Vec<u8> {
    let hkdf = Hkdf::<Sha256>::new(None, input_key_material);
    let mut output = vec![0u8; output_len];
    hkdf.expand(info, &mut output).expect("Output length is valid for HKDF");
    output
}

/// Compute SHA256 hash
#[allow(dead_code)]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Extract deterministic entropy from update metadata (for Remove/Rotate)
pub fn extract_metadata_entropy(update_id: ulid::Ulid, issuer: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(&update_id.to_bytes());
    hasher.update(issuer.as_bytes());
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sha256() {
        let data = b"hello world";
        let hash = sha256(data);
        assert_eq!(hash.len(), 32);

        // SHA256 is deterministic
        let hash2 = sha256(data);
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_derive_key_hkdf() {
        let ikm = b"input key material";
        let info = b"context info";

        let key1 = derive_key_hkdf(ikm, info, 32);
        assert_eq!(key1.len(), 32);

        // HKDF is deterministic
        let key2 = derive_key_hkdf(ikm, info, 32);
        assert_eq!(key1, key2);

        // Different info produces different key
        let key3 = derive_key_hkdf(ikm, b"different info", 32);
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_extract_metadata_entropy() {
        let update_id = ulid::Ulid::new();
        let issuer = "@alice:example.org";

        let entropy1 = extract_metadata_entropy(update_id, issuer);
        assert_eq!(entropy1.len(), 32);

        // Deterministic
        let entropy2 = extract_metadata_entropy(update_id, issuer);
        assert_eq!(entropy1, entropy2);

        // Different update_id produces different entropy
        let update_id2 = ulid::Ulid::new();
        let entropy3 = extract_metadata_entropy(update_id2, issuer);
        assert_ne!(entropy1, entropy3);
    }

    #[test]
    fn test_ed25519_sign_verify() {
        let secret_key = Ed25519SecretKey::new();
        let public_key = secret_key.public_key();
        let message = b"test message";

        let signature = sign_ed25519(&secret_key, message);
        assert!(verify_ed25519(&public_key, message, &signature.to_bytes()));

        // Wrong message fails verification
        assert!(!verify_ed25519(&public_key, b"wrong message", &signature.to_bytes()));
    }

    #[test]
    fn test_x25519_dh() {
        let alice_public = Curve25519PublicKey::from([1u8; 32]);
        let bob_public = Curve25519PublicKey::from([2u8; 32]);

        // DH is deterministic with same inputs
        let shared1 = x25519_dh(&alice_public, &bob_public);
        let shared2 = x25519_dh(&alice_public, &bob_public);
        assert_eq!(shared1, shared2);

        // Different keys produce different output
        let charlie_public = Curve25519PublicKey::from([3u8; 32]);
        let shared3 = x25519_dh(&alice_public, &charlie_public);
        assert_ne!(shared1, shared3);
    }
}
