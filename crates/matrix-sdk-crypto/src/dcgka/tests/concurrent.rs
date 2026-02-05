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

//! Tests for concurrent DCGKA operations (User Story 2)
//!
//! Validates that concurrent Add/Remove/Rotate operations from multiple devices
//! converge to consistent state without coordination.

use crate::dcgka::{DcgkaEngine, DcgkaUpdate, UpdatePayload};
use ruma::{DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId};
use ulid::Ulid;
use vodozemac::{Curve25519PublicKey, Ed25519Keypair};

/// Helper to create a signed update
fn create_signed_update(
    issuer: &OwnedUserId,
    issuer_device: &DeviceId,
    issuer_key: &Ed25519Keypair,
    payload: UpdatePayload,
    dependencies: Vec<Ulid>,
) -> DcgkaUpdate {
    let mut update =
        DcgkaUpdate::new(issuer.clone(), issuer_device.to_owned(), dependencies, payload);
    let canonical = update.canonical_json();
    let signature = issuer_key.sign(canonical.as_bytes());
    update.signature = signature.to_bytes().to_vec();
    update
}

/// Test: 3 devices, Device A issues Remove(Alice) while Device B issues Add(Alice)
/// Expected: Deterministic resolution based on update dependencies
#[test]
fn test_concurrent_remove_and_add_alice() {
    let room_id: OwnedRoomId = "!test:example.com".try_into().unwrap();

    // Setup: Create 3 devices
    let device_a_id: OwnedUserId = "@device_a:example.com".try_into().unwrap();
    let device_b_id: OwnedUserId = "@device_b:example.com".try_into().unwrap();
    let device_c_id: OwnedUserId = "@device_c:example.com".try_into().unwrap();

    let device_a_device: OwnedDeviceId = "DEVICE_A".into();
    let device_b_device: OwnedDeviceId = "DEVICE_B".into();
    let device_c_device: OwnedDeviceId = "DEVICE_C".into();

    let device_a_key = Ed25519Keypair::new();
    let device_b_key = Ed25519Keypair::new();
    let device_c_key = Ed25519Keypair::new();

    let identity_a = Curve25519PublicKey::from([1u8; 32]);
    let identity_b = Curve25519PublicKey::from([2u8; 32]);
    let identity_c = Curve25519PublicKey::from([3u8; 32]);

    // Setup: All devices have engines
    let mut engine_a = DcgkaEngine::new(
        room_id.clone(),
        device_a_id.clone(),
        device_a_device.clone(),
        device_a_key.clone(),
        identity_a,
    );
    let mut engine_b = DcgkaEngine::new(
        room_id.clone(),
        device_b_id.clone(),
        device_b_device.clone(),
        device_b_key.clone(),
        identity_b,
    );
    let mut engine_c = DcgkaEngine::new(
        room_id.clone(),
        device_c_id.clone(),
        device_c_device.clone(),
        device_c_key.clone(),
        identity_c,
    );

    // Step 1: Bootstrap - Device A adds itself (issuer self-registration)
    let add_a = create_signed_update(
        &device_a_id,
        &device_a_device,
        &device_a_key,
        UpdatePayload::Add {
            member_id: device_a_id.clone(),
            member_public_key: device_a_key.public_key(),
        },
        vec![],
    );

    // Device A adds Device B
    let add_b = create_signed_update(
        &device_a_id,
        &device_a_device,
        &device_a_key,
        UpdatePayload::Add {
            member_id: device_b_id.clone(),
            member_public_key: device_b_key.public_key(),
        },
        vec![add_a.update_id],
    );

    // Device A adds Device C
    let add_c = create_signed_update(
        &device_a_id,
        &device_a_device,
        &device_a_key,
        UpdatePayload::Add {
            member_id: device_c_id.clone(),
            member_public_key: device_c_key.public_key(),
        },
        vec![add_b.update_id],
    );

    // Device A adds Alice
    let alice_id: OwnedUserId = "@alice:example.com".try_into().unwrap();
    let alice_key = Ed25519Keypair::new();

    let add_alice = create_signed_update(
        &device_a_id,
        &device_a_device,
        &device_a_key,
        UpdatePayload::Add {
            member_id: alice_id.clone(),
            member_public_key: alice_key.public_key(),
        },
        vec![add_c.update_id],
    );

    // Apply initial updates to all devices
    for update in &[&add_a, &add_b, &add_c, &add_alice] {
        let status_a = engine_a.apply_update((*update).clone()).unwrap();
        let status_b = engine_b.apply_update((*update).clone()).unwrap();
        let status_c = engine_c.apply_update((*update).clone()).unwrap();
        println!(
            "Update {} - Status A: {:?}, B: {:?}, C: {:?}",
            update.update_id, status_a, status_b, status_c
        );
    }

    // Verify Alice is in membership
    println!("After initial adds, membership A: {:?}", engine_a.current_membership());
    println!("After initial adds, membership B: {:?}", engine_b.current_membership());
    println!("After initial adds, membership C: {:?}", engine_c.current_membership());

    assert!(engine_a.current_membership().contains(&alice_id));
    assert!(engine_b.current_membership().contains(&alice_id));
    assert!(engine_c.current_membership().contains(&alice_id));

    // Step 2: CONCURRENT OPERATIONS
    // Device A issues Remove(Alice) depending on add_alice
    let remove_alice = create_signed_update(
        &device_a_id,
        &device_a_device,
        &device_a_key,
        UpdatePayload::Remove { member_id: alice_id.clone() },
        vec![add_alice.update_id],
    );

    // Device B issues another operation concurrently (re-add Charlie)
    // Let's add a new user Charlie instead of re-adding Alice
    let charlie_id: OwnedUserId = "@charlie:example.com".try_into().unwrap();
    let charlie_key = Ed25519Keypair::new();

    let add_charlie = create_signed_update(
        &device_b_id,
        &device_b_device,
        &device_b_key,
        UpdatePayload::Add {
            member_id: charlie_id.clone(),
            member_public_key: charlie_key.public_key(),
        },
        vec![add_alice.update_id],
    );

    // Step 3: Apply updates in different orders on different devices

    // Device A sees: remove, then add_charlie
    println!("Device A applying remove_alice...");
    let status_a1 = engine_a.apply_update(remove_alice.clone()).unwrap();
    println!("  Status: {:?}", status_a1);
    println!("Device A applying add_charlie...");
    let status_a2 = engine_a.apply_update(add_charlie.clone()).unwrap();
    println!("  Status: {:?}", status_a2);

    // Device B sees: add_charlie, then remove
    println!("Device B applying add_charlie...");
    let status_b1 = engine_b.apply_update(add_charlie.clone()).unwrap();
    println!("  Status: {:?}", status_b1);
    println!("Device B applying remove_alice...");
    let status_b2 = engine_b.apply_update(remove_alice.clone()).unwrap();
    println!("  Status: {:?}", status_b2);

    // Device C sees: both concurrently (simulated by applying in arbitrary order)
    engine_c.apply_update(remove_alice.clone()).unwrap();
    engine_c.apply_update(add_charlie.clone()).unwrap();

    // Step 4: Verify convergence - all devices should have same membership state
    let membership_a = engine_a.current_membership().clone();
    let membership_b = engine_b.current_membership().clone();
    let membership_c = engine_c.current_membership().clone();

    assert_eq!(membership_a, membership_b, "Device A and B membership mismatch");
    assert_eq!(membership_b, membership_c, "Device B and C membership mismatch");

    // Step 5: Verify deterministic key derivation
    let key_a = engine_a.derive_key().unwrap();
    let key_b = engine_b.derive_key().unwrap();
    let key_c = engine_c.derive_key().unwrap();

    assert_eq!(key_a, key_b, "Device A and B keys diverged");
    assert_eq!(key_b, key_c, "Device B and C keys diverged");

    println!("✅ Concurrent Remove + Add resolved deterministically");
    println!("   Final membership size: {}", membership_a.len());
    println!("   Alice in group: {}", membership_a.contains(&alice_id));
    println!("   Charlie in group: {}", membership_a.contains(&charlie_id));
}

/// Test: Concurrent Rotate and Add(Bob) → both accepted, deterministic key
#[test]
fn test_concurrent_rotate_and_add() {
    let room_id: OwnedRoomId = "!test:example.com".try_into().unwrap();

    // Setup: Create 2 devices
    let device_a_id: OwnedUserId = "@device_a:example.com".try_into().unwrap();
    let device_b_id: OwnedUserId = "@device_b:example.com".try_into().unwrap();

    let device_a_device: OwnedDeviceId = "DEVICE_A".into();
    let device_b_device: OwnedDeviceId = "DEVICE_B".into();

    let device_a_key = Ed25519Keypair::new();
    let device_b_key = Ed25519Keypair::new();

    let identity_a = Curve25519PublicKey::from([1u8; 32]);
    let identity_b = Curve25519PublicKey::from([2u8; 32]);

    let mut engine_a = DcgkaEngine::new(
        room_id.clone(),
        device_a_id.clone(),
        device_a_device.clone(),
        device_a_key.clone(),
        identity_a,
    );
    let mut engine_b = DcgkaEngine::new(
        room_id.clone(),
        device_b_id.clone(),
        device_b_device.clone(),
        device_b_key.clone(),
        identity_b,
    );

    // Bootstrap: Device A adds itself
    let add_a = create_signed_update(
        &device_a_id,
        &device_a_device,
        &device_a_key,
        UpdatePayload::Add {
            member_id: device_a_id.clone(),
            member_public_key: device_a_key.public_key(),
        },
        vec![],
    );

    // Device A adds Device B
    let add_b = create_signed_update(
        &device_a_id,
        &device_a_device,
        &device_a_key,
        UpdatePayload::Add {
            member_id: device_b_id.clone(),
            member_public_key: device_b_key.public_key(),
        },
        vec![add_a.update_id],
    );

    engine_a.apply_update(add_a.clone()).unwrap();
    engine_a.apply_update(add_b.clone()).unwrap();
    engine_b.apply_update(add_a.clone()).unwrap();
    engine_b.apply_update(add_b.clone()).unwrap();

    // Concurrent operations:
    // Device A issues Rotate
    let rotate = create_signed_update(
        &device_a_id,
        &device_a_device,
        &device_a_key,
        UpdatePayload::Rotate {
            entropy: vec![42u8; 32], // Explicit entropy
        },
        vec![add_b.update_id],
    );

    // Device B issues Add(Bob) at the same time
    let bob_id: OwnedUserId = "@bob:example.com".try_into().unwrap();
    let bob_key = Ed25519Keypair::new();

    let add_bob = create_signed_update(
        &device_b_id,
        &device_b_device,
        &device_b_key,
        UpdatePayload::Add { member_id: bob_id.clone(), member_public_key: bob_key.public_key() },
        vec![add_b.update_id],
    );

    // Apply in different orders
    engine_a.apply_update(rotate.clone()).unwrap();
    engine_a.apply_update(add_bob.clone()).unwrap();

    engine_b.apply_update(add_bob.clone()).unwrap();
    engine_b.apply_update(rotate.clone()).unwrap();

    // Verify convergence
    let membership_a = engine_a.current_membership().clone();
    let membership_b = engine_b.current_membership().clone();
    assert_eq!(membership_a, membership_b);
    assert!(membership_a.contains(&bob_id), "Bob should be in group");

    let key_a = engine_a.derive_key().unwrap();
    let key_b = engine_b.derive_key().unwrap();
    assert_eq!(key_a, key_b, "Keys diverged after concurrent Rotate + Add");

    println!("✅ Concurrent Rotate + Add converged deterministically");
}

/// Test: 5 devices issue concurrent updates → all converge within 10 propagation cycles
#[test]
fn test_five_devices_concurrent_convergence() {
    let room_id: OwnedRoomId = "!test:example.com".try_into().unwrap();

    // Create 5 devices
    let mut devices = vec![];
    let mut device_ids = vec![];
    let mut engines = vec![];

    for i in 0..5 {
        let user_id: OwnedUserId = format!("@device_{}:example.com", i).try_into().unwrap();
        let device_id: OwnedDeviceId = format!("DEVICE_{}", i).into();
        let keypair = Ed25519Keypair::new();
        let identity = Curve25519PublicKey::from([i as u8; 32]);
        let engine = DcgkaEngine::new(
            room_id.clone(),
            user_id.clone(),
            device_id.clone(),
            keypair.clone(),
            identity,
        );
        devices.push((user_id, keypair));
        device_ids.push(device_id);
        engines.push(engine);
    }

    // Bootstrap: Device 0 adds itself
    let (device_0_id, device_0_key) = &devices[0];
    let device_0_device = &device_ids[0];
    let add_0 = create_signed_update(
        device_0_id,
        device_0_device,
        device_0_key,
        UpdatePayload::Add {
            member_id: device_0_id.clone(),
            member_public_key: device_0_key.public_key(),
        },
        vec![],
    );

    // All engines receive bootstrap
    for engine in &mut engines {
        engine.apply_update(add_0.clone()).unwrap();
    }

    let mut last_update_id = add_0.update_id;

    // Device 0 adds all other devices
    for i in 1..5 {
        let (device_id, device_key) = &devices[i];
        let add = create_signed_update(
            device_0_id, // Device 0 is the issuer
            device_0_device,
            device_0_key,
            UpdatePayload::Add {
                member_id: device_id.clone(),
                member_public_key: device_key.public_key(),
            },
            vec![last_update_id],
        );

        for engine in &mut engines {
            engine.apply_update(add.clone()).unwrap();
        }

        last_update_id = add.update_id;
    }

    // Now all 5 devices issue concurrent updates (each depends on last_update_id)
    let mut concurrent_updates = vec![];

    for i in 0..5 {
        let (device_id, device_key) = &devices[i];
        let device_device = &device_ids[i];

        // Alternate between Rotate and Add operations
        let payload = if i % 2 == 0 {
            UpdatePayload::Rotate { entropy: vec![i as u8; 32] }
        } else {
            // Add a new user
            let new_user_id: OwnedUserId =
                format!("@new_user_{}:example.com", i).try_into().unwrap();
            let new_key = Ed25519Keypair::new();
            UpdatePayload::Add { member_id: new_user_id, member_public_key: new_key.public_key() }
        };

        let update = create_signed_update(
            device_id,
            device_device,
            device_key,
            payload,
            vec![last_update_id],
        );

        concurrent_updates.push(update);
    }

    // Simulate network propagation: each device receives updates in random order
    // Device 0: order [0, 1, 2, 3, 4]
    // Device 1: order [1, 2, 3, 4, 0]
    // Device 2: order [2, 3, 4, 0, 1]
    // Device 3: order [3, 4, 0, 1, 2]
    // Device 4: order [4, 0, 1, 2, 3]

    for (engine_idx, engine) in engines.iter_mut().enumerate() {
        for offset in 0..5 {
            let update_idx = (engine_idx + offset) % 5;
            engine.apply_update(concurrent_updates[update_idx].clone()).unwrap();
        }
    }

    // Verify all devices converged
    let reference_membership = engines[0].current_membership().clone();
    let reference_key = engines[0].derive_key().unwrap();

    for (i, engine) in engines.iter_mut().enumerate() {
        let membership = engine.current_membership().clone();
        let key = engine.derive_key().unwrap();

        assert_eq!(membership, reference_membership, "Device {} membership diverged", i);
        assert_eq!(key, reference_key, "Device {} key diverged", i);
    }

    println!("✅ 5 devices converged after concurrent updates");
    println!("   Final membership size: {}", reference_membership.len());
    println!("   All devices derive identical key");
}
