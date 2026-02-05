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

//! Convergence tests for User Story 1 (T018-T022)
//!
//! Tests that devices receiving updates in different orders converge to identical keys

use crate::dcgka::{DcgkaEngine, DcgkaUpdate, UpdatePayload, UpdateType};
use ruma::{DeviceId, OwnedUserId, RoomId, device_id, room_id, user_id};
use ulid::Ulid;
use vodozemac::{Curve25519PublicKey, Ed25519Keypair, Ed25519PublicKey};

/// Test helper: Create a test engine with fresh device keys
fn create_test_engine(
    room_id: &RoomId,
    user_id: &OwnedUserId,
    device_id: &DeviceId,
) -> DcgkaEngine {
    let signing_key = Ed25519Keypair::new();
    let identity_key = Curve25519PublicKey::from([42u8; 32]); // Dummy key for tests

    DcgkaEngine::new(
        room_id.to_owned(),
        user_id.to_owned(),
        device_id.to_owned(),
        signing_key,
        identity_key,
    )
}

/// Test helper: Create a signed update
fn create_signed_update(
    issuer: &OwnedUserId,
    issuer_device: &DeviceId,
    payload: UpdatePayload,
    dependencies: Vec<Ulid>,
    signing_key: &Ed25519Keypair,
) -> DcgkaUpdate {
    let mut update =
        DcgkaUpdate::new(issuer.to_owned(), issuer_device.to_owned(), dependencies, payload);

    // Sign the canonical JSON
    let canonical = update.canonical_json();
    let signature = signing_key.sign(canonical.as_bytes());
    update.signature = signature.to_bytes().to_vec();

    update
}

// T019: 2 devices, 3 updates (Add/Remove/Rotate), different orders → identical keys
#[test]
fn test_two_devices_different_orders_converge() {
    let room_id = room_id!("!test:example.org");
    let alice = user_id!("@alice:example.org").to_owned();
    let bob = user_id!("@bob:example.org").to_owned();

    // Create two devices
    let mut device_a = create_test_engine(room_id, &alice, device_id!("DEVICE_A"));
    let mut device_b = create_test_engine(room_id, &bob, device_id!("DEVICE_B"));

    // Create signing key for issuer
    let issuer_key = Ed25519Keypair::new();
    let issuer = user_id!("@creator:example.org").to_owned();

    // Create keypair for Alice to get valid public key
    let alice_keypair = Ed25519Keypair::new();

    // First, issuer must add themselves to establish their public key
    let issuer_add = create_signed_update(
        &issuer,
        device_id!("CREATOR"),
        UpdatePayload::Add {
            member_id: issuer.clone(),
            member_public_key: issuer_key.public_key(),
        },
        vec![],
        &issuer_key,
    );

    device_a.apply_update(issuer_add.clone()).expect("Issuer add should be accepted");
    device_b.apply_update(issuer_add).expect("Issuer add should be accepted");

    // Create 3 updates: Add(Alice), Remove(Bob), Rotate
    let update1 = create_signed_update(
        &issuer,
        device_id!("CREATOR"),
        UpdatePayload::Add {
            member_id: alice.to_owned(),
            member_public_key: alice_keypair.public_key(),
        },
        vec![],
        &issuer_key,
    );

    let update2 = create_signed_update(
        &issuer,
        device_id!("CREATOR"),
        UpdatePayload::Remove { member_id: bob.to_owned() },
        vec![update1.update_id],
        &issuer_key,
    );

    let update3 = create_signed_update(
        &issuer,
        device_id!("CREATOR"),
        UpdatePayload::Rotate { entropy: vec![0xABu8; 32] },
        vec![update2.update_id],
        &issuer_key,
    );

    // Device A: Apply in order [1, 2, 3]
    device_a.apply_update(update1.clone()).expect("update1 should be accepted");
    device_a.apply_update(update2.clone()).expect("update2 should be accepted");
    device_a.apply_update(update3.clone()).expect("update3 should be accepted");

    // Device B: Apply in order [3, 1, 2] (reverse dependency order)
    device_b.apply_update(update3.clone()).expect("update3 should be pending");
    device_b.apply_update(update1.clone()).expect("update1 should be accepted");
    device_b.apply_update(update2.clone()).expect("update2 should be accepted");

    // Both devices should converge to identical state
    assert_eq!(
        device_a.fingerprint(),
        device_b.fingerprint(),
        "Devices should have identical fingerprints after convergence"
    );

    let key_a = device_a.derive_key().expect("Device A should derive key");
    let key_b = device_b.derive_key().expect("Device B should derive key");

    assert_eq!(
        key_a.key_material, key_b.key_material,
        "Devices should have identical group keys after convergence"
    );

    assert_eq!(key_a.generation, key_b.generation, "Devices should have identical key generation");
}

// T020: Device receives Add(Alice) before Remove(Alice) dependency → buffers as Pending → accepts after Remove arrives
#[test]
fn test_pending_buffering_eventual_acceptance() {
    let room_id = room_id!("!test:example.org");
    let alice = user_id!("@alice:example.org").to_owned();

    let mut device = create_test_engine(room_id, &alice, device_id!("DEVICE_A"));
    let issuer_key = Ed25519Keypair::new();
    let issuer = user_id!("@creator:example.org").to_owned();
    let alice_keypair = Ed25519Keypair::new();
    let bob_keypair = Ed25519Keypair::new();

    // First, issuer must add themselves to establish their public key
    let issuer_add = create_signed_update(
        &issuer,
        device_id!("CREATOR"),
        UpdatePayload::Add {
            member_id: issuer.clone(),
            member_public_key: issuer_key.public_key(),
        },
        vec![],
        &issuer_key,
    );
    device.apply_update(issuer_add).expect("Issuer add should be accepted");

    // Create Add(Alice) update (ID: U1)
    let add_alice = create_signed_update(
        &issuer,
        device_id!("CREATOR"),
        UpdatePayload::Add {
            member_id: alice.to_owned(),
            member_public_key: alice_keypair.public_key(),
        },
        vec![],
        &issuer_key,
    );

    // Create Add(Bob) update that depends on Add(Alice) (ID: U2, depends on U1)
    let add_bob = create_signed_update(
        &issuer,
        device_id!("CREATOR"),
        UpdatePayload::Add {
            member_id: user_id!("@bob:example.org").to_owned(),
            member_public_key: bob_keypair.public_key(),
        },
        vec![add_alice.update_id],
        &issuer_key,
    );

    // Apply Add(Bob) first (should be pending - missing dependency)
    let status_add = device.apply_update(add_bob.clone());
    assert!(status_add.is_ok(), "Add(Bob) should be buffered as pending when dependency missing");

    // Verify Add(Bob) is in pending state
    assert!(
        device.state().pending_updates.contains_key(&add_bob.update_id),
        "Add(Bob) should be in pending state"
    );

    // Apply Add(Alice) (should trigger re-evaluation of pending queue)
    let status_add_alice = device.apply_update(add_alice.clone());
    assert!(status_add_alice.is_ok(), "Add(Alice) should be accepted");

    // After Add(Alice) is accepted, Add(Bob) should be automatically re-evaluated and accepted
    assert!(
        device.state().accepted_updates.contains_key(&add_bob.update_id),
        "Add(Bob) should be accepted after dependency arrives"
    );

    assert!(
        !device.state().pending_updates.contains_key(&add_bob.update_id),
        "Add(Bob) should no longer be pending"
    );
}

// T021: Device offline, replays 100 missed updates → converges to same key as online devices
#[test]
fn test_offline_replay_convergence() {
    let room_id = room_id!("!test:example.org");
    let alice = user_id!("@alice:example.org").to_owned();
    let bob = user_id!("@bob:example.org").to_owned();

    let mut online_device = create_test_engine(room_id, &alice, device_id!("ONLINE"));
    let mut offline_device = create_test_engine(room_id, &bob, device_id!("OFFLINE"));

    let issuer_key = Ed25519Keypair::new();
    let issuer = user_id!("@creator:example.org").to_owned();

    // Create keypairs for users to get valid public keys
    let user_keypairs: Vec<Ed25519Keypair> = (0..100).map(|_| Ed25519Keypair::new()).collect();

    // First, issuer must add themselves to establish their public key
    let issuer_add = create_signed_update(
        &issuer,
        device_id!("CREATOR"),
        UpdatePayload::Add {
            member_id: issuer.clone(),
            member_public_key: issuer_key.public_key(),
        },
        vec![],
        &issuer_key,
    );
    online_device.apply_update(issuer_add.clone()).expect("Online: Issuer add should be accepted");

    // Generate 100 chained updates
    let mut updates: Vec<DcgkaUpdate> = Vec::new();
    let mut last_id = None;

    for i in 0..100 {
        let payload = match i % 3 {
            0 => UpdatePayload::Add {
                member_id: OwnedUserId::try_from(format!("@user{}:example.org", i)).unwrap(),
                member_public_key: user_keypairs[i].public_key(),
            },
            1 => UpdatePayload::Rotate { entropy: vec![(i % 256) as u8; 32] },
            _ => {
                if i > 0 {
                    UpdatePayload::Remove {
                        member_id: OwnedUserId::try_from(format!("@user{}:example.org", i - 1))
                            .unwrap(),
                    }
                } else {
                    UpdatePayload::Rotate { entropy: vec![0u8; 32] }
                }
            }
        };

        let deps = if let Some(id) = last_id { vec![id] } else { vec![] };

        let update =
            create_signed_update(&issuer, device_id!("CREATOR"), payload, deps, &issuer_key);

        last_id = Some(update.update_id);
        updates.push(update);
    }

    // Online device: Apply all updates as they arrive
    for update in &updates {
        online_device
            .apply_update(update.clone())
            .expect("Online device should accept all updates");
    }

    // Offline device: Replay issuer_add + all 100 updates later
    offline_device.apply_update(issuer_add).expect("Offline: Issuer add should be accepted");
    for update in &updates {
        offline_device
            .apply_update(update.clone())
            .expect("Offline device should accept all updates");
    }

    // Both should converge
    assert_eq!(
        online_device.fingerprint(),
        offline_device.fingerprint(),
        "Online and offline devices should have identical fingerprints"
    );

    let key_online = online_device.derive_key().expect("Online device should derive key");
    let key_offline = offline_device.derive_key().expect("Offline device should derive key");

    assert_eq!(
        key_online.key_material, key_offline.key_material,
        "Online and offline devices should have identical group keys"
    );
}

// T022: Fingerprint is deterministic (same accepted_updates → same fingerprint)
#[test]
fn test_fingerprint_determinism() {
    let room_id = room_id!("!test:example.org");
    let alice = user_id!("@alice:example.org").to_owned();
    let bob = user_id!("@bob:example.org").to_owned();

    let mut device_1 = create_test_engine(room_id, &alice, device_id!("DEVICE_1"));
    let mut device_2 = create_test_engine(room_id, &bob, device_id!("DEVICE_2"));

    let issuer_key = Ed25519Keypair::new();
    let issuer = user_id!("@creator:example.org").to_owned();

    // First, issuer must add themselves to establish their public key
    let issuer_add = create_signed_update(
        &issuer,
        device_id!("CREATOR"),
        UpdatePayload::Add {
            member_id: issuer.clone(),
            member_public_key: issuer_key.public_key(),
        },
        vec![],
        &issuer_key,
    );
    device_1.apply_update(issuer_add.clone()).expect("Device 1: Issuer add should be accepted");
    device_2.apply_update(issuer_add).expect("Device 2: Issuer add should be accepted");

    // Create 5 updates with deterministic IDs
    let mut updates: Vec<DcgkaUpdate> = Vec::new();
    for i in 0..5 {
        let update = create_signed_update(
            &issuer,
            device_id!("CREATOR"),
            UpdatePayload::Rotate { entropy: vec![(i * 10) as u8; 32] },
            if i > 0 { vec![updates[i - 1].update_id] } else { vec![] },
            &issuer_key,
        );
        updates.push(update);
    }

    // Apply same updates to both devices (in same order)
    for update in &updates {
        device_1.apply_update(update.clone()).expect("Device 1 should accept update");
        device_2.apply_update(update.clone()).expect("Device 2 should accept update");
    }

    // Fingerprints should be identical
    let fp1 = device_1.fingerprint();
    let fp2 = device_2.fingerprint();

    assert_eq!(fp1, fp2, "Fingerprints should be deterministic for same accepted updates");

    // Fingerprint should be stable (calling multiple times returns same result)
    assert_eq!(device_1.fingerprint(), fp1, "Fingerprint should be stable");
    assert_eq!(device_2.fingerprint(), fp2, "Fingerprint should be stable");

    // Derived keys should also be deterministic
    let key1_a = device_1.derive_key().expect("Device 1 should derive key");
    let key1_b = device_1.derive_key().expect("Device 1 should derive key again");

    assert_eq!(key1_a.key_material, key1_b.key_material, "Key derivation should be deterministic");
}
