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

//! DCGKA state management

use std::collections::{HashMap, HashSet};

use ruma::{OwnedRoomId, OwnedUserId};
use serde::{Deserialize, Serialize};
use ulid::Ulid;

use super::update::{DcgkaUpdate, UpdateType};

/// DCGKA state tracking accepted/pending/rejected updates and current membership
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DcgkaState {
    /// Matrix room this state belongs to
    pub room_id: OwnedRoomId,

    /// Updates that passed validation and all dependencies satisfied
    pub accepted_updates: HashMap<Ulid, DcgkaUpdate>,

    /// Updates awaiting dependency resolution or validation
    pub pending_updates: HashMap<Ulid, DcgkaUpdate>,

    /// Updates that failed validation (terminal state)
    pub rejected_updates: HashMap<Ulid, DcgkaUpdate>,

    /// Current group membership (derived from accepted updates)
    #[serde(skip)]
    current_membership: Option<HashSet<OwnedUserId>>,

    /// Accumulated entropy from accepted updates (for key derivation)
    #[serde(skip)]
    entropy_pool: Option<Vec<u8>>,
}

impl DcgkaState {
    /// Create a new empty DCGKA state for a room
    pub fn new(room_id: OwnedRoomId) -> Self {
        Self {
            room_id,
            accepted_updates: HashMap::new(),
            pending_updates: HashMap::new(),
            rejected_updates: HashMap::new(),
            current_membership: Some(HashSet::new()),
            entropy_pool: Some(Vec::new()),
        }
    }

    /// Get current group membership (lazy computed)
    pub fn current_membership(&mut self) -> &HashSet<OwnedUserId> {
        if self.current_membership.is_none() {
            self.current_membership = Some(self.derive_membership());
        }
        self.current_membership.as_ref().unwrap()
    }

    /// Derive membership from accepted updates
    fn derive_membership(&self) -> HashSet<OwnedUserId> {
        let mut members = HashSet::new();

        // Get topological order of accepted updates
        let ordered_updates = self.topological_sort_updates(&self.accepted_updates);

        for update in ordered_updates {
            match update.update_type() {
                UpdateType::Add => {
                    if let Some(member_id) = update.payload.member_id() {
                        members.insert(member_id.clone());
                    }
                }
                UpdateType::Remove => {
                    if let Some(member_id) = update.payload.member_id() {
                        members.remove(member_id);
                    }
                }
                UpdateType::Rotate => {
                    // No membership change
                }
            }
        }

        members
    }

    /// Topological sort of updates by dependencies (internal helper)
    fn topological_sort_updates<'a>(
        &'a self,
        updates: &'a HashMap<Ulid, DcgkaUpdate>,
    ) -> Vec<&'a DcgkaUpdate> {
        let mut sorted = Vec::new();
        let mut visited = HashSet::new();
        let mut temp_mark = HashSet::new();

        fn visit<'a>(
            update_id: &Ulid,
            updates: &'a HashMap<Ulid, DcgkaUpdate>,
            visited: &mut HashSet<Ulid>,
            temp_mark: &mut HashSet<Ulid>,
            sorted: &mut Vec<&'a DcgkaUpdate>,
        ) -> bool {
            if visited.contains(update_id) {
                return true;
            }
            if temp_mark.contains(update_id) {
                // Cycle detected
                return false;
            }

            if let Some(update) = updates.get(update_id) {
                temp_mark.insert(*update_id);

                for dep_id in &update.dependencies {
                    if !visit(dep_id, updates, visited, temp_mark, sorted) {
                        return false;
                    }
                }

                temp_mark.remove(update_id);
                visited.insert(*update_id);
                sorted.push(update);
            }

            true
        }

        for update_id in updates.keys() {
            if !visited.contains(update_id) {
                visit(update_id, updates, &mut visited, &mut temp_mark, &mut sorted);
            }
        }

        sorted
    }

    /// Public topological sort that returns update IDs in dependency order
    pub fn topological_sort(&self) -> Result<Vec<Ulid>, String> {
        let mut sorted_ids = Vec::new();
        let mut visited = HashSet::new();
        let mut temp_mark = HashSet::new();

        fn visit(
            update_id: &Ulid,
            updates: &HashMap<Ulid, DcgkaUpdate>,
            visited: &mut HashSet<Ulid>,
            temp_mark: &mut HashSet<Ulid>,
            sorted: &mut Vec<Ulid>,
        ) -> Result<(), String> {
            if visited.contains(update_id) {
                return Ok(());
            }
            if temp_mark.contains(update_id) {
                // Cycle detected
                return Err("Circular dependency detected".to_string());
            }

            if let Some(update) = updates.get(update_id) {
                temp_mark.insert(*update_id);

                for dep_id in &update.dependencies {
                    visit(dep_id, updates, visited, temp_mark, sorted)?;
                }

                temp_mark.remove(update_id);
                visited.insert(*update_id);
                sorted.push(*update_id);
            }

            Ok(())
        }

        for update_id in self.accepted_updates.keys() {
            if !visited.contains(update_id) {
                visit(
                    update_id,
                    &self.accepted_updates,
                    &mut visited,
                    &mut temp_mark,
                    &mut sorted_ids,
                )?;
            }
        }

        // Sort the result by ULID to ensure deterministic ordering across devices
        // This is critical because HashMap iteration order is non-deterministic
        sorted_ids.sort();

        Ok(sorted_ids)
    }

    /// Mark an update as accepted (takes ownership of update)
    pub fn mark_accepted(&mut self, update_id: Ulid, update: DcgkaUpdate) {
        self.accepted_updates.insert(update_id, update);
        self.pending_updates.remove(&update_id);
        // Invalidate cached membership
        self.current_membership = None;
        self.entropy_pool = None;
    }

    /// Mark an update as rejected (takes ownership of update)
    pub fn mark_rejected(&mut self, update_id: Ulid, update: DcgkaUpdate) {
        self.rejected_updates.insert(update_id, update);
        self.pending_updates.remove(&update_id);
    }

    /// Add an update to pending state
    pub fn add_pending(&mut self, update: DcgkaUpdate) -> bool {
        let update_id = update.update_id;

        // Check if already processed
        if self.accepted_updates.contains_key(&update_id)
            || self.pending_updates.contains_key(&update_id)
            || self.rejected_updates.contains_key(&update_id)
        {
            return false; // Idempotent: already processed
        }

        self.pending_updates.insert(update_id, update);
        true
    }

    /// Check if all dependencies of an update are in accepted state
    pub fn dependencies_satisfied(&self, update: &DcgkaUpdate) -> bool {
        update.dependencies.iter().all(|dep_id| self.accepted_updates.contains_key(dep_id))
    }

    /// Check if any dependency is rejected
    pub fn has_rejected_dependency(&self, update: &DcgkaUpdate) -> bool {
        update.dependencies.iter().any(|dep_id| self.rejected_updates.contains_key(dep_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcgka::UpdatePayload;

    #[test]
    fn test_new_state() {
        let room_id = ruma::room_id!("!test:example.org").to_owned();
        let mut state = DcgkaState::new(room_id.clone());

        assert_eq!(state.room_id, room_id);
        assert!(state.accepted_updates.is_empty());
        assert!(state.pending_updates.is_empty());
        assert!(state.rejected_updates.is_empty());
        assert!(state.current_membership().is_empty());
    }

    #[test]
    fn test_derive_membership() {
        let room_id = ruma::room_id!("!test:example.org").to_owned();
        let mut state = DcgkaState::new(room_id);

        let alice = ruma::user_id!("@alice:example.org").to_owned();
        let bob = ruma::user_id!("@bob:example.org").to_owned();
        let device = ruma::device_id!("DEVICE").to_owned();

        // Generate valid Ed25519 keypairs
        let alice_keypair = vodozemac::Ed25519Keypair::new();
        let bob_keypair = vodozemac::Ed25519Keypair::new();

        // Add Alice
        let mut add_alice = DcgkaUpdate::new(
            alice.clone(),
            device.clone(),
            vec![],
            UpdatePayload::Add {
                member_id: alice.clone(),
                member_public_key: alice_keypair.public_key(),
            },
        );
        // Sign the update
        let canonical_json = add_alice.canonical_json();
        add_alice.signature = alice_keypair.sign(canonical_json.as_bytes()).to_bytes().to_vec();

        state.accepted_updates.insert(add_alice.update_id, add_alice);

        // Add Bob
        let mut add_bob = DcgkaUpdate::new(
            alice.clone(),
            device.clone(),
            vec![],
            UpdatePayload::Add {
                member_id: bob.clone(),
                member_public_key: bob_keypair.public_key(),
            },
        );
        // Sign the update with Alice's key (Alice is adding Bob)
        let canonical_json = add_bob.canonical_json();
        add_bob.signature = alice_keypair.sign(canonical_json.as_bytes()).to_bytes().to_vec();

        state.accepted_updates.insert(add_bob.update_id, add_bob);

        // Call derive_membership directly since we're bypassing validation
        let membership = state.derive_membership();
        assert_eq!(membership.len(), 2);
        assert!(membership.contains(&alice));
        assert!(membership.contains(&bob));
    }

    #[test]
    fn test_add_pending() {
        let room_id = ruma::room_id!("!test:example.org").to_owned();
        let mut state = DcgkaState::new(room_id);

        let update = DcgkaUpdate::new(
            ruma::user_id!("@alice:example.org").to_owned(),
            ruma::device_id!("DEVICE").to_owned(),
            vec![],
            UpdatePayload::Rotate { entropy: vec![1u8; 32] },
        );
        let update_id = update.update_id;

        assert!(state.add_pending(update.clone()));
        assert!(state.pending_updates.contains_key(&update_id));

        // Idempotent: adding again returns false
        assert!(!state.add_pending(update));
    }

    #[test]
    fn test_mark_accepted() {
        let room_id = ruma::room_id!("!test:example.org").to_owned();
        let mut state = DcgkaState::new(room_id);

        let update = DcgkaUpdate::new(
            ruma::user_id!("@alice:example.org").to_owned(),
            ruma::device_id!("DEVICE").to_owned(),
            vec![],
            UpdatePayload::Rotate { entropy: vec![1u8; 32] },
        );
        let update_id = update.update_id;

        state.add_pending(update.clone());
        state.mark_accepted(update_id, update);

        assert!(state.accepted_updates.contains_key(&update_id));
        assert!(!state.pending_updates.contains_key(&update_id));
    }

    #[test]
    fn test_mark_rejected() {
        let room_id = ruma::room_id!("!test:example.org").to_owned();
        let mut state = DcgkaState::new(room_id);

        let update = DcgkaUpdate::new(
            ruma::user_id!("@alice:example.org").to_owned(),
            ruma::device_id!("DEVICE").to_owned(),
            vec![],
            UpdatePayload::Rotate { entropy: vec![1u8; 32] },
        );
        let update_id = update.update_id;

        state.add_pending(update.clone());
        state.mark_rejected(update_id, update);

        assert!(state.rejected_updates.contains_key(&update_id));
        assert!(!state.pending_updates.contains_key(&update_id));
    }

    #[test]
    fn test_dependencies_satisfied() {
        let room_id = ruma::room_id!("!test:example.org").to_owned();
        let mut state = DcgkaState::new(room_id);

        let update1 = DcgkaUpdate::new(
            ruma::user_id!("@alice:example.org").to_owned(),
            ruma::device_id!("DEVICE").to_owned(),
            vec![],
            UpdatePayload::Rotate { entropy: vec![1u8; 32] },
        );
        let update1_id = update1.update_id;

        let update2 = DcgkaUpdate::new(
            ruma::user_id!("@alice:example.org").to_owned(),
            ruma::device_id!("DEVICE").to_owned(),
            vec![update1_id],
            UpdatePayload::Rotate { entropy: vec![2u8; 32] },
        );

        // Before update1 is accepted
        assert!(!state.dependencies_satisfied(&update2));

        // After update1 is accepted
        state.mark_accepted(update1_id, update1);
        assert!(state.dependencies_satisfied(&update2));
    }
}
