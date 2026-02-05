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

//! Validation tests for signature and dependency checking (User Story 3)
//!
//! Tests that invalid updates are immediately rejected and that dependent
//! updates are also rejected (dependency poisoning).

use crate::dcgka::{DcgkaEngine, DcgkaUpdate, UpdatePayload, UpdateStatus};
use ruma::{DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, device_id, room_id, user_id};
use ulid::Ulid;
use vodozemac::{Curve25519PublicKey, Ed25519Keypair};

/// Helper to create a test engine
fn create_test_engine(
    room_id: &OwnedRoomId,
    user_id: &OwnedUserId,
    device_id: &DeviceId,
) -> DcgkaEngine {
    let signing_key = Ed25519Keypair::new();
    let identity_key = Curve25519PublicKey::from([42u8; 32]);

    DcgkaEngine::new(
        room_id.clone(),
        user_id.clone(),
        device_id.to_owned(),
        signing_key,
        identity_key,
    )
}

/// Helper to create a properly signed update
fn create_signed_update(
    issuer: &OwnedUserId,
    issuer_device: &DeviceId,
    payload: UpdatePayload,
    dependencies: Vec<Ulid>,
    signing_key: &Ed25519Keypair,
) -> DcgkaUpdate {
    let mut update =
        DcgkaUpdate::new(issuer.clone(), issuer_device.to_owned(), dependencies, payload);

    let canonical = update.canonical_json();
    let signature = signing_key.sign(canonical.as_bytes());
    update.signature = signature.to_bytes().to_vec();

    update
}

/// Test: Invalid Ed25519 signature → immediate Rejected state (T053)
#[test]
fn test_invalid_signature_rejected() {
    let room_id = room_id!("!test:example.org").to_owned();
    let creator = user_id!("@creator:example.org").to_owned();
    let creator_device = device_id!("CREATOR");

    let mut engine = create_test_engine(&room_id, &creator, creator_device);

    let creator_key = Ed25519Keypair::new();

    // First, creator adds themselves (valid)
    let creator_add = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add {
            member_id: creator.clone(),
            member_public_key: creator_key.public_key(),
        },
        vec![],
        &creator_key,
    );

    let status = engine.apply_update(creator_add.clone()).unwrap();
    assert_eq!(status, UpdateStatus::Accepted, "Bootstrap update should be accepted");

    // Create an update with INVALID signature (wrong signing key)
    let wrong_key = Ed25519Keypair::new(); // Different key!
    let alice = user_id!("@alice:example.org").to_owned();
    let alice_key = Ed25519Keypair::new();

    let invalid_update = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add { member_id: alice.clone(), member_public_key: alice_key.public_key() },
        vec![creator_add.update_id],
        &wrong_key, // Wrong key for signature!
    );

    // Apply the invalid update
    let status = engine.apply_update(invalid_update.clone()).unwrap();

    // Should be immediately rejected
    assert_eq!(status, UpdateStatus::Rejected, "Update with invalid signature should be rejected");

    // Verify Alice is NOT in membership
    assert!(
        !engine.current_membership().contains(&alice),
        "Alice should not be in group after invalid update"
    );

    println!("✅ Invalid signature correctly rejected");
}

/// Test: Update depends on Rejected update → also Rejected (poisoned dependency) (T054)
#[test]
fn test_dependency_poisoning() {
    let room_id = room_id!("!test:example.org").to_owned();
    let creator = user_id!("@creator:example.org").to_owned();
    let creator_device = device_id!("CREATOR");

    let mut engine = create_test_engine(&room_id, &creator, creator_device);

    let creator_key = Ed25519Keypair::new();

    // Bootstrap: Creator adds themselves
    let creator_add = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add {
            member_id: creator.clone(),
            member_public_key: creator_key.public_key(),
        },
        vec![],
        &creator_key,
    );

    engine.apply_update(creator_add.clone()).unwrap();

    // Create INVALID update (wrong signature)
    let wrong_key = Ed25519Keypair::new();
    let alice = user_id!("@alice:example.org").to_owned();
    let alice_key = Ed25519Keypair::new();

    let invalid_add_alice = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add { member_id: alice.clone(), member_public_key: alice_key.public_key() },
        vec![creator_add.update_id],
        &wrong_key, // Wrong key!
    );

    let status1 = engine.apply_update(invalid_add_alice.clone()).unwrap();
    assert_eq!(status1, UpdateStatus::Rejected, "Invalid update should be rejected");

    // Now create a VALID update that DEPENDS on the rejected update
    let bob = user_id!("@bob:example.org").to_owned();
    let bob_key = Ed25519Keypair::new();

    let add_bob = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add { member_id: bob.clone(), member_public_key: bob_key.public_key() },
        vec![invalid_add_alice.update_id], // Depends on rejected update!
        &creator_key,                      // Correct signature
    );

    let status2 = engine.apply_update(add_bob).unwrap();

    // Should be rejected due to poisoned dependency
    assert_eq!(
        status2,
        UpdateStatus::Rejected,
        "Update depending on rejected update should also be rejected"
    );

    // Verify neither Alice nor Bob are in membership
    assert!(!engine.current_membership().contains(&alice), "Alice should not be in group");
    assert!(
        !engine.current_membership().contains(&bob),
        "Bob should not be in group due to poisoned dependency"
    );

    println!("✅ Dependency poisoning works correctly");
}

/// Test: Rejected update never transitions to Accepted (terminal state) (T055)
#[test]
fn test_rejected_is_terminal() {
    let room_id = room_id!("!test:example.org").to_owned();
    let creator = user_id!("@creator:example.org").to_owned();
    let creator_device = device_id!("CREATOR");

    let mut engine = create_test_engine(&room_id, &creator, creator_device);

    let creator_key = Ed25519Keypair::new();

    // Bootstrap
    let creator_add = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add {
            member_id: creator.clone(),
            member_public_key: creator_key.public_key(),
        },
        vec![],
        &creator_key,
    );

    engine.apply_update(creator_add.clone()).unwrap();

    // Create invalid update
    let wrong_key = Ed25519Keypair::new();
    let alice = user_id!("@alice:example.org").to_owned();
    let alice_key = Ed25519Keypair::new();

    let invalid_update = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add { member_id: alice.clone(), member_public_key: alice_key.public_key() },
        vec![creator_add.update_id],
        &wrong_key,
    );

    let status1 = engine.apply_update(invalid_update.clone()).unwrap();
    assert_eq!(status1, UpdateStatus::Rejected);

    // Try to apply the SAME update again (should remain rejected)
    let status2 = engine.apply_update(invalid_update.clone()).unwrap();
    assert_eq!(status2, UpdateStatus::Rejected, "Rejected update should remain rejected");

    // Try to apply a corrected version with same update_id (shouldn't change state)
    // Note: In real implementation, update_id is unique, so this tests idempotency
    let status3 = engine.apply_update(invalid_update).unwrap();
    assert_eq!(status3, UpdateStatus::Rejected, "Rejected state is terminal");

    println!("✅ Rejected state is terminal (no transitions to Accepted)");
}

/// Test: 100% detection rate for invalid signatures (T056)
#[test]
fn test_perfect_invalid_signature_detection() {
    let room_id = room_id!("!test:example.org").to_owned();
    let creator = user_id!("@creator:example.org").to_owned();
    let creator_device = device_id!("CREATOR");

    let mut engine = create_test_engine(&room_id, &creator, creator_device);

    let creator_key = Ed25519Keypair::new();

    // Bootstrap
    let creator_add = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add {
            member_id: creator.clone(),
            member_public_key: creator_key.public_key(),
        },
        vec![],
        &creator_key,
    );

    engine.apply_update(creator_add.clone()).unwrap();

    // Test 100 updates with invalid signatures - all should be rejected
    let mut rejected_count = 0;
    let total_tests = 100;

    for i in 0..total_tests {
        let wrong_key = Ed25519Keypair::new(); // Different key each time
        let user_id: OwnedUserId = format!("@user_{}:example.org", i).try_into().unwrap();
        let user_key = Ed25519Keypair::new();

        let invalid_update = create_signed_update(
            &creator,
            creator_device,
            UpdatePayload::Add { member_id: user_id, member_public_key: user_key.public_key() },
            vec![creator_add.update_id],
            &wrong_key, // Wrong signature
        );

        let status = engine.apply_update(invalid_update).unwrap();

        if status == UpdateStatus::Rejected {
            rejected_count += 1;
        }
    }

    // 100% detection rate
    assert_eq!(rejected_count, total_tests, "All invalid signatures should be detected");

    // Verify only creator is in membership
    assert_eq!(engine.current_membership().len(), 1, "Only creator should be in group");
    assert!(engine.current_membership().contains(&creator));

    println!("✅ Perfect invalid signature detection: {}/{} rejected", rejected_count, total_tests);
}

/// Test: Non-member issuer → Rejected (VR-002)
#[test]
fn test_non_member_issuer_rejected() {
    let room_id = room_id!("!test:example.org").to_owned();
    let creator = user_id!("@creator:example.org").to_owned();
    let creator_device = device_id!("CREATOR");

    let mut engine = create_test_engine(&room_id, &creator, creator_device);

    let creator_key = Ed25519Keypair::new();

    // Bootstrap
    let creator_add = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add {
            member_id: creator.clone(),
            member_public_key: creator_key.public_key(),
        },
        vec![],
        &creator_key,
    );

    engine.apply_update(creator_add.clone()).unwrap();

    // Attacker (not a member) tries to add themselves
    let attacker = user_id!("@attacker:example.org").to_owned();
    let attacker_device = device_id!("ATTACKER");
    let attacker_key = Ed25519Keypair::new();

    let attacker_add = create_signed_update(
        &attacker,
        attacker_device,
        UpdatePayload::Add {
            member_id: attacker.clone(),
            member_public_key: attacker_key.public_key(),
        },
        vec![creator_add.update_id],
        &attacker_key, // Valid signature, but issuer not a member
    );

    let status = engine.apply_update(attacker_add).unwrap();

    assert_eq!(status, UpdateStatus::Rejected, "Non-member issuer should be rejected");
    assert!(!engine.current_membership().contains(&attacker), "Attacker should not be in group");

    println!("✅ Non-member issuer correctly rejected");
}

/// Test: Duplicate Add → Rejected (VR-003)
#[test]
fn test_duplicate_add_rejected() {
    let room_id = room_id!("!test:example.org").to_owned();
    let creator = user_id!("@creator:example.org").to_owned();
    let creator_device = device_id!("CREATOR");

    let mut engine = create_test_engine(&room_id, &creator, creator_device);

    let creator_key = Ed25519Keypair::new();

    // Bootstrap
    let creator_add = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add {
            member_id: creator.clone(),
            member_public_key: creator_key.public_key(),
        },
        vec![],
        &creator_key,
    );

    engine.apply_update(creator_add.clone()).unwrap();

    // Add Alice
    let alice = user_id!("@alice:example.org").to_owned();
    let alice_key = Ed25519Keypair::new();

    let add_alice = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add { member_id: alice.clone(), member_public_key: alice_key.public_key() },
        vec![creator_add.update_id],
        &creator_key,
    );

    let status1 = engine.apply_update(add_alice.clone()).unwrap();
    assert_eq!(status1, UpdateStatus::Accepted, "First add should be accepted");

    // Try to add Alice AGAIN (duplicate)
    let add_alice_again = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add { member_id: alice.clone(), member_public_key: alice_key.public_key() },
        vec![add_alice.update_id],
        &creator_key,
    );

    let status2 = engine.apply_update(add_alice_again).unwrap();

    assert_eq!(status2, UpdateStatus::Rejected, "Duplicate add should be rejected");
    assert_eq!(engine.current_membership().len(), 2, "Should still have 2 members");

    println!("✅ Duplicate add correctly rejected");
}

/// Test: Remove non-member → Rejected (VR-004)
#[test]
fn test_remove_non_member_rejected() {
    let room_id = room_id!("!test:example.org").to_owned();
    let creator = user_id!("@creator:example.org").to_owned();
    let creator_device = device_id!("CREATOR");

    let mut engine = create_test_engine(&room_id, &creator, creator_device);

    let creator_key = Ed25519Keypair::new();

    // Bootstrap
    let creator_add = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Add {
            member_id: creator.clone(),
            member_public_key: creator_key.public_key(),
        },
        vec![],
        &creator_key,
    );

    engine.apply_update(creator_add.clone()).unwrap();

    // Try to remove someone who was never added
    let alice = user_id!("@alice:example.org").to_owned();

    let remove_alice = create_signed_update(
        &creator,
        creator_device,
        UpdatePayload::Remove { member_id: alice.clone() },
        vec![creator_add.update_id],
        &creator_key,
    );

    let status = engine.apply_update(remove_alice).unwrap();

    assert_eq!(status, UpdateStatus::Rejected, "Remove non-member should be rejected");
    assert_eq!(engine.current_membership().len(), 1, "Should still have 1 member");

    println!("✅ Remove non-member correctly rejected");
}
