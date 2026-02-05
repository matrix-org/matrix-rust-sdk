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

//! DCGKA engine for managing update application and key derivation

use std::collections::HashSet;

use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, AeadCore, OsRng},
};
use ruma::{OwnedDeviceId, OwnedRoomId, OwnedUserId};
use sha2::{Digest, Sha256};
use ulid::Ulid;
use vodozemac::{Curve25519PublicKey, Ed25519Keypair, Ed25519PublicKey};

use super::{
    crypto::{GroupKey, derive_key_hkdf, extract_metadata_entropy, verify_ed25519},
    state::DcgkaState,
    update::{DcgkaUpdate, UpdatePayload},
};

/// Wrapper for Ed25519Keypair to implement Debug
#[derive(Clone)]
pub struct SigningKey(Ed25519Keypair);

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SigningKey").finish_non_exhaustive()
    }
}

impl SigningKey {
    pub fn new(keypair: Ed25519Keypair) -> Self {
        Self(keypair)
    }

    pub fn sign(&self, message: &[u8]) -> vodozemac::Ed25519Signature {
        self.0.sign(message)
    }
}

/// Stateful DCGKA engine for processing updates and deriving group keys
#[derive(Debug, Clone)]
pub struct DcgkaEngine {
    /// Room ID for this DCGKA instance
    room_id: OwnedRoomId,

    /// Local user ID
    user_id: OwnedUserId,

    /// Local device ID
    device_id: OwnedDeviceId,

    /// Current DCGKA state
    state: DcgkaState,

    /// Local device signing key
    signing_key: SigningKey,

    /// Local device identity key (for X25519 DH)
    identity_key: Curve25519PublicKey,

    /// Current derived group key (cached)
    current_key: Option<GroupKey>,

    /// Historical group keys for decrypting old messages
    historical_keys: Vec<GroupKey>,

    /// Member public keys cache (user_id → Ed25519PublicKey)
    member_keys: std::collections::HashMap<OwnedUserId, Ed25519PublicKey>,
}

impl DcgkaEngine {
    /// Create a new DCGKA engine for a room (T023-T024)
    pub fn new(
        room_id: OwnedRoomId,
        user_id: OwnedUserId,
        device_id: OwnedDeviceId,
        signing_key: Ed25519Keypair,
        identity_key: Curve25519PublicKey,
    ) -> Self {
        Self {
            room_id: room_id.clone(),
            user_id,
            device_id,
            state: DcgkaState::new(room_id),
            signing_key: SigningKey::new(signing_key),
            identity_key,
            current_key: None,
            historical_keys: Vec::new(),
            member_keys: std::collections::HashMap::new(),
        }
    }

    /// Get reference to current state
    pub fn state(&self) -> &DcgkaState {
        &self.state
    }

    /// Get current membership (convenience method)
    pub fn current_membership(&mut self) -> &HashSet<OwnedUserId> {
        self.state.current_membership()
    }

    /// Apply an update to the DCGKA state (T029-T030)
    ///
    /// Returns whether the update was accepted, rejected, or remains pending
    pub fn apply_update(&mut self, update: DcgkaUpdate) -> Result<UpdateStatus, DcgkaError> {
        let update_id = update.update_id;

        // Skip if already processed
        if self.state.accepted_updates.contains_key(&update_id)
            || self.state.rejected_updates.contains_key(&update_id)
        {
            return Ok(if self.state.accepted_updates.contains_key(&update_id) {
                UpdateStatus::Accepted
            } else {
                UpdateStatus::Rejected
            });
        }

        // Check for rejected dependencies FIRST (dependency poisoning)
        if self.state.has_rejected_dependency(&update) {
            self.state.mark_rejected(update_id, update);
            return Ok(UpdateStatus::Rejected);
        }

        // T026: Check dependencies
        if !self.check_dependencies(&update) {
            self.state.add_pending(update);
            return Ok(UpdateStatus::Pending);
        }

        // T027: Validate signature
        if !self.validate_signature(&update)? {
            self.state.mark_rejected(update_id, update);
            return Ok(UpdateStatus::Rejected);
        }

        // T028: Validate membership
        if !self.validate_membership(&update)? {
            self.state.mark_rejected(update_id, update);
            return Ok(UpdateStatus::Rejected);
        }

        // Accept the update
        self.accept_update(update)?;

        // T030: Re-evaluate pending queue
        self.reevaluate_pending()?;

        // T066: Store current key in historical_keys before invalidating
        if let Some(ref current_key) = self.current_key {
            self.historical_keys.push(current_key.clone());

            // Limit to last 100 keys (configurable, per spec)
            const MAX_HISTORICAL_KEYS: usize = 100;
            if self.historical_keys.len() > MAX_HISTORICAL_KEYS {
                // Remove oldest key (first element)
                self.historical_keys.remove(0);
            }
        }

        // Invalidate cached key
        self.current_key = None;

        Ok(UpdateStatus::Accepted)
    }

    /// Check if all dependencies are satisfied (T026)
    fn check_dependencies(&self, update: &DcgkaUpdate) -> bool {
        self.state.dependencies_satisfied(update)
    }

    /// Validate update signature (T027)
    fn validate_signature(&self, update: &DcgkaUpdate) -> Result<bool, DcgkaError> {
        // Get issuer's public key from member_keys cache or from Add payload
        let issuer_key = if let Some(key) = self.member_keys.get(&update.issuer) {
            *key
        } else if let UpdatePayload::Add { member_id, member_public_key } = &update.payload {
            if member_id == &update.issuer {
                *member_public_key
            } else {
                return Err(DcgkaError::SignatureValidationFailed);
            }
        } else {
            return Err(DcgkaError::SignatureValidationFailed);
        };

        // Verify signature on canonical JSON
        let canonical = update.canonical_json();
        Ok(verify_ed25519(&issuer_key, canonical.as_bytes(), &update.signature))
    }

    /// Validate membership constraints (T028)
    fn validate_membership(&mut self, update: &DcgkaUpdate) -> Result<bool, DcgkaError> {
        let current_membership = self.state.current_membership().clone();

        match &update.payload {
            UpdatePayload::Add { member_id, .. } => {
                // Issuer must be a current member (or this is first update)
                if !current_membership.is_empty() && !current_membership.contains(&update.issuer) {
                    return Ok(false);
                }
                // Cannot add duplicate members
                if current_membership.contains(member_id) {
                    return Ok(false);
                }
                Ok(true)
            }
            UpdatePayload::Remove { member_id } => {
                // Issuer must be a current member
                if !current_membership.contains(&update.issuer) {
                    return Ok(false);
                }
                // Can only remove current members
                if !current_membership.contains(member_id) {
                    return Ok(false);
                }
                Ok(true)
            }
            UpdatePayload::Rotate { .. } => {
                // Issuer must be a current member
                Ok(current_membership.is_empty() || current_membership.contains(&update.issuer))
            }
        }
    }

    /// Accept an update into the state
    fn accept_update(&mut self, update: DcgkaUpdate) -> Result<(), DcgkaError> {
        let update_id = update.update_id;

        // Cache member key if this is an Add update
        if let UpdatePayload::Add { member_id, member_public_key } = &update.payload {
            self.member_keys.insert(member_id.clone(), *member_public_key);
        }

        // Mark as accepted
        self.state.mark_accepted(update_id, update);

        Ok(())
    }

    /// Re-evaluate pending queue after accepting an update (T030)
    fn reevaluate_pending(&mut self) -> Result<(), DcgkaError> {
        let mut to_process = Vec::new();

        // Collect pending updates that now have satisfied dependencies
        for (id, update) in &self.state.pending_updates {
            if self.state.dependencies_satisfied(update) {
                to_process.push((*id, update.clone()));
            }
        }

        // Process each pending update
        for (id, update) in to_process {
            // Remove from pending
            self.state.pending_updates.remove(&id);

            // Recursively apply (this will handle validation and further re-evaluation)
            self.apply_update(update)?;
        }

        Ok(())
    }

    /// Derive the current group encryption key from accepted updates (T031-T034)
    pub fn derive_key(&mut self) -> Result<GroupKey, DcgkaError> {
        // Return cached key if available
        if let Some(ref key) = self.current_key {
            return Ok(key.clone());
        }

        // Get topologically sorted accepted updates
        let sorted_updates = self.state.topological_sort()?;

        if sorted_updates.is_empty() {
            return Err(DcgkaError::NoAcceptedUpdates);
        }

        // Accumulate entropy from all accepted updates
        let mut entropy_pool = Vec::new();

        for update_id in &sorted_updates {
            if let Some(update) = self.state.accepted_updates.get(update_id) {
                let entropy = self.extract_update_entropy(update)?;
                entropy_pool.extend_from_slice(&entropy);
            }
        }

        // Derive key using HKDF
        let key_material = derive_key_hkdf(&entropy_pool, b"DCGKA_GROUP_KEY", 32);

        let generation = sorted_updates.len() as u64;
        let group_key = GroupKey::new(
            key_material.try_into().map_err(|_| DcgkaError::KeyDerivationFailed)?,
            generation,
        );

        // Cache the derived key
        self.current_key = Some(group_key.clone());

        Ok(group_key)
    }

    /// Extract entropy from a single update (T032-T034)
    fn extract_update_entropy(&self, update: &DcgkaUpdate) -> Result<Vec<u8>, DcgkaError> {
        match &update.payload {
            UpdatePayload::Add { member_public_key, .. } => {
                // T032: Use member's public key as entropy source (deterministic)
                // For PoC: Hash the public key + update metadata for deterministic entropy
                let mut hasher = Sha256::new();
                hasher.update(member_public_key.as_bytes());
                hasher.update(&update.update_id.to_bytes());
                hasher.update(update.issuer.as_bytes());
                Ok(hasher.finalize().to_vec())
            }
            UpdatePayload::Remove { .. } => {
                // T033: Deterministic entropy from metadata only
                Ok(extract_metadata_entropy(update.update_id, update.issuer.as_str()))
            }
            UpdatePayload::Rotate { entropy } => {
                // T034: User-provided entropy + metadata
                let metadata_entropy =
                    extract_metadata_entropy(update.update_id, update.issuer.as_str());
                let mut combined = entropy.clone();
                combined.extend_from_slice(&metadata_entropy);

                Ok(combined)
            }
        }
    }

    /// Compute state fingerprint for convergence testing (T035)
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();

        // Get sorted accepted update IDs
        let mut sorted_ids: Vec<Ulid> = self.state.accepted_updates.keys().copied().collect();
        sorted_ids.sort();

        // Hash all update IDs in order
        for id in sorted_ids {
            hasher.update(&id.to_bytes());
        }

        hasher.finalize().into()
    }

    /// Encrypt plaintext with current group key (T064)
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, DcgkaError> {
        // Derive current group key if needed
        let group_key = if let Some(ref key) = self.current_key {
            key.clone()
        } else {
            let key = self.derive_key()?;
            self.current_key = Some(key.clone());
            key
        };

        // Create ChaCha20-Poly1305 cipher
        let cipher = ChaCha20Poly1305::new(&group_key.key_material.into());

        // Generate random nonce (96 bits for ChaCha20-Poly1305)
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);

        // Encrypt plaintext with AEAD
        let ciphertext =
            cipher.encrypt(&nonce, plaintext).map_err(|_| DcgkaError::EncryptionFailed)?;

        // Prepend nonce to ciphertext (nonce || ciphertext)
        let mut result = Vec::with_capacity(nonce.len() + ciphertext.len());
        result.extend_from_slice(&nonce);
        result.extend_from_slice(&ciphertext);

        Ok(result)
    }

    /// Decrypt ciphertext (tries current + historical keys) (T065, T067)
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>, DcgkaError> {
        // Extract nonce (first 12 bytes)
        if ciphertext.len() < 12 {
            return Err(DcgkaError::DecryptionFailed);
        }

        let (nonce_bytes, encrypted) = ciphertext.split_at(12);
        let nonce = chacha20poly1305::Nonce::from_slice(nonce_bytes);

        // Try current key first (if available)
        if let Some(ref current_key) = self.current_key {
            if let Ok(plaintext) = self.try_decrypt_with_key(current_key, nonce, encrypted) {
                return Ok(plaintext);
            }
        }

        // Try historical keys (most recent first) (T067)
        for historical_key in self.historical_keys.iter().rev() {
            if let Ok(plaintext) = self.try_decrypt_with_key(historical_key, nonce, encrypted) {
                return Ok(plaintext);
            }
        }

        Err(DcgkaError::DecryptionFailed)
    }

    /// Try to decrypt with a specific key
    fn try_decrypt_with_key(
        &self,
        key: &GroupKey,
        nonce: &chacha20poly1305::Nonce,
        ciphertext: &[u8],
    ) -> Result<Vec<u8>, DcgkaError> {
        let cipher = ChaCha20Poly1305::new(&key.key_material.into());
        cipher.decrypt(nonce, ciphertext).map_err(|_| DcgkaError::DecryptionFailed)
    }
}

/// Status of an update after application
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Update accepted (all dependencies satisfied, validation passed)
    Accepted,
    /// Update pending (awaiting dependencies or validation)
    Pending,
    /// Update rejected (validation failed, terminal state)
    Rejected,
}

/// DCGKA-specific errors (T036)
#[derive(Debug, Clone, thiserror::Error)]
pub enum DcgkaError {
    #[error("Signature validation failed")]
    SignatureValidationFailed,

    #[error("Membership validation failed")]
    MembershipValidationFailed,

    #[error("Circular dependency detected in update graph")]
    CircularDependency,

    #[error("No accepted updates available for key derivation")]
    NoAcceptedUpdates,

    #[error("Key derivation failed")]
    KeyDerivationFailed,

    #[error("Invalid update payload")]
    InvalidPayload,

    #[error("Topological sort error: {0}")]
    TopologicalSortError(String),

    #[error("Encryption failed")]
    EncryptionFailed,

    #[error("Decryption failed")]
    DecryptionFailed,

    #[error("Invalid state: {0}")]
    InvalidState(String),
}

impl From<String> for DcgkaError {
    fn from(error: String) -> Self {
        DcgkaError::TopologicalSortError(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruma::{device_id, room_id, user_id};
    use vodozemac::{Curve25519PublicKey, Ed25519Keypair};

    #[test]
    fn test_engine_creation() {
        let room_id = room_id!("!test:example.org").to_owned();
        let user_id = user_id!("@alice:example.org").to_owned();
        let device_id = device_id!("DEVICE").to_owned();

        let signing_key = Ed25519Keypair::new();
        let identity_key = Curve25519PublicKey::from([42u8; 32]);

        let engine =
            DcgkaEngine::new(room_id.clone(), user_id, device_id, signing_key, identity_key);

        assert_eq!(engine.state.accepted_updates.len(), 0);
        assert_eq!(engine.state.pending_updates.len(), 0);
        assert_eq!(engine.state.room_id, room_id);
    }
}
