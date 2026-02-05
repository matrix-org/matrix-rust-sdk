//! Integration tests for DCGKA with OlmMachine
//!
//! Tests the full integration of DCGKA with OlmMachine, CryptoStore, and
//! Matrix event handling.

use std::sync::Arc;

use matrix_sdk_test::async_test;
use ruma::{OwnedDeviceId, OwnedUserId, device_id, room_id, user_id};
use vodozemac::Ed25519Keypair;

use crate::{
    dcgka::{DcgkaEngine, DcgkaUpdate, UpdatePayload, UpdateStatus},
    machine::OlmMachine,
    store::MemoryStore,
};

/// Helper to create an OlmMachine for testing
async fn create_test_machine(user_id: &ruma::UserId, device_id: &ruma::DeviceId) -> OlmMachine {
    OlmMachine::new(user_id, device_id).await
}

#[async_test]
async fn test_olm_machine_dcgka_encrypt_decrypt() {
    // Create two OlmMachines representing two devices
    let alice_machine =
        create_test_machine(user_id!("@alice:example.org"), device_id!("ALICE_DEVICE")).await;
    let bob_machine =
        create_test_machine(user_id!("@bob:example.org"), device_id!("BOB_DEVICE")).await;

    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org");
    let plaintext = b"Hello DCGKA!";

    // First, Alice needs to add herself to the group
    let alice_keypair = Ed25519Keypair::new();
    let mut alice_add = DcgkaUpdate::new(
        alice_id.to_owned(),
        device_id!("ALICE_DEVICE").to_owned(),
        vec![],
        UpdatePayload::Add {
            member_id: alice_id.to_owned(),
            member_public_key: alice_keypair.public_key(),
        },
    );
    let canonical = alice_add.canonical_json();
    alice_add.signature = alice_keypair.sign(canonical.as_bytes()).to_bytes().to_vec();

    // Process the update
    alice_machine
        .handle_dcgka_event(room_id, alice_add.clone())
        .await
        .expect("Alice should process her own update");

    // Alice encrypts a message
    let ciphertext = alice_machine
        .dcgka_encrypt(room_id, plaintext)
        .await
        .expect("Alice should encrypt successfully");

    // Alice can decrypt her own message
    let decrypted = alice_machine
        .dcgka_decrypt(room_id, &ciphertext)
        .await
        .expect("Alice should decrypt her own message");

    assert_eq!(decrypted, plaintext, "Alice's decrypt should match original");
}

#[async_test]
async fn test_olm_machine_handle_dcgka_event() {
    // Create two OlmMachines
    let alice_machine =
        create_test_machine(user_id!("@alice:example.org"), device_id!("ALICE_DEVICE")).await;
    let bob_machine =
        create_test_machine(user_id!("@bob:example.org"), device_id!("BOB_DEVICE")).await;

    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org");
    let bob_id = user_id!("@bob:example.org");

    // Create a DCGKA update (Alice adding herself)
    let alice_keypair = Ed25519Keypair::new();
    let mut alice_add = DcgkaUpdate::new(
        alice_id.to_owned(),
        device_id!("ALICE_DEVICE").to_owned(),
        vec![],
        UpdatePayload::Add {
            member_id: alice_id.to_owned(),
            member_public_key: alice_keypair.public_key(),
        },
    );

    // Sign the update
    let canonical_json = alice_add.canonical_json();
    alice_add.signature = alice_keypair.sign(canonical_json.as_bytes()).to_bytes().to_vec();

    // Alice processes her own update
    let status = alice_machine
        .handle_dcgka_event(room_id, alice_add.clone())
        .await
        .expect("Alice should process her own update");

    assert!(matches!(status, UpdateStatus::Accepted), "Alice's self-add should be accepted");

    // Bob processes Alice's update
    let status = bob_machine
        .handle_dcgka_event(room_id, alice_add)
        .await
        .expect("Bob should process Alice's update");

    assert!(matches!(status, UpdateStatus::Accepted), "Bob should accept Alice's update");
}

#[async_test]
async fn test_olm_machine_multi_device_convergence() {
    // Create three OlmMachines
    let alice =
        create_test_machine(user_id!("@alice:example.org"), device_id!("ALICE_DEVICE")).await;
    let bob = create_test_machine(user_id!("@bob:example.org"), device_id!("BOB_DEVICE")).await;
    let charlie =
        create_test_machine(user_id!("@charlie:example.org"), device_id!("CHARLIE_DEVICE")).await;

    let room_id = room_id!("!test:example.org");

    // Create updates for all three members
    let alice_id = user_id!("@alice:example.org");
    let bob_id = user_id!("@bob:example.org");
    let charlie_id = user_id!("@charlie:example.org");

    let alice_keypair = Ed25519Keypair::new();
    let bob_keypair = Ed25519Keypair::new();
    let charlie_keypair = Ed25519Keypair::new();

    // Alice adds herself
    let mut alice_add = DcgkaUpdate::new(
        alice_id.to_owned(),
        device_id!("ALICE_DEVICE").to_owned(),
        vec![],
        UpdatePayload::Add {
            member_id: alice_id.to_owned(),
            member_public_key: alice_keypair.public_key(),
        },
    );
    let canonical = alice_add.canonical_json();
    alice_add.signature = alice_keypair.sign(canonical.as_bytes()).to_bytes().to_vec();

    // Alice adds Bob (depends on alice_add)
    let mut bob_add = DcgkaUpdate::new(
        alice_id.to_owned(),
        device_id!("ALICE_DEVICE").to_owned(),
        vec![alice_add.update_id],
        UpdatePayload::Add {
            member_id: bob_id.to_owned(),
            member_public_key: bob_keypair.public_key(),
        },
    );
    let canonical = bob_add.canonical_json();
    bob_add.signature = alice_keypair.sign(canonical.as_bytes()).to_bytes().to_vec();

    // Bob adds Charlie (depends on bob_add)
    let mut charlie_add = DcgkaUpdate::new(
        bob_id.to_owned(),
        device_id!("BOB_DEVICE").to_owned(),
        vec![bob_add.update_id],
        UpdatePayload::Add {
            member_id: charlie_id.to_owned(),
            member_public_key: charlie_keypair.public_key(),
        },
    );
    let canonical = charlie_add.canonical_json();
    charlie_add.signature = bob_keypair.sign(canonical.as_bytes()).to_bytes().to_vec();

    // Process updates in different orders
    // Alice: correct order
    alice.handle_dcgka_event(room_id, alice_add.clone()).await.unwrap();
    alice.handle_dcgka_event(room_id, bob_add.clone()).await.unwrap();
    alice.handle_dcgka_event(room_id, charlie_add.clone()).await.unwrap();

    // Bob: reverse order (should buffer and eventually accept)
    bob.handle_dcgka_event(room_id, charlie_add.clone()).await.unwrap();
    bob.handle_dcgka_event(room_id, bob_add.clone()).await.unwrap();
    bob.handle_dcgka_event(room_id, alice_add.clone()).await.unwrap();

    // Charlie: mixed order
    charlie.handle_dcgka_event(room_id, bob_add.clone()).await.unwrap();
    charlie.handle_dcgka_event(room_id, alice_add.clone()).await.unwrap();
    charlie.handle_dcgka_event(room_id, charlie_add.clone()).await.unwrap();

    // All devices should converge to the same state
    // Encrypt/decrypt test to verify convergence
    let plaintext = b"Convergence test message";

    let alice_encrypted = alice.dcgka_encrypt(room_id, plaintext).await.unwrap();
    let bob_encrypted = bob.dcgka_encrypt(room_id, plaintext).await.unwrap();
    let charlie_encrypted = charlie.dcgka_encrypt(room_id, plaintext).await.unwrap();

    // Each device can decrypt messages from all other devices
    assert_eq!(
        alice.dcgka_decrypt(room_id, &bob_encrypted).await.unwrap(),
        plaintext,
        "Alice should decrypt Bob's message"
    );
    assert_eq!(
        bob.dcgka_decrypt(room_id, &charlie_encrypted).await.unwrap(),
        plaintext,
        "Bob should decrypt Charlie's message"
    );
    assert_eq!(
        charlie.dcgka_decrypt(room_id, &alice_encrypted).await.unwrap(),
        plaintext,
        "Charlie should decrypt Alice's message"
    );
}

#[async_test]
async fn test_olm_machine_persistence() {
    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org");
    let plaintext = b"Test persistence";

    let ciphertext = {
        // Create Alice's machine and encrypt
        let alice =
            create_test_machine(user_id!("@alice:example.org"), device_id!("ALICE_DEVICE")).await;

        // Create and process an update
        let alice_keypair = Ed25519Keypair::new();
        let mut alice_add = DcgkaUpdate::new(
            alice_id.to_owned(),
            device_id!("ALICE_DEVICE").to_owned(),
            vec![],
            UpdatePayload::Add {
                member_id: alice_id.to_owned(),
                member_public_key: alice_keypair.public_key(),
            },
        );
        let canonical = alice_add.canonical_json();
        alice_add.signature = alice_keypair.sign(canonical.as_bytes()).to_bytes().to_vec();

        alice.handle_dcgka_event(room_id, alice_add).await.unwrap();

        // Encrypt a message
        alice.dcgka_encrypt(room_id, plaintext).await.unwrap()
    };

    // The machine is dropped here, simulating a restart

    // Create a new machine with the same identity
    // Note: In a real scenario, we'd use a persistent store
    // For PoC with MemoryStore, this demonstrates the API pattern
    let alice2 =
        create_test_machine(user_id!("@alice:example.org"), device_id!("ALICE_DEVICE")).await;

    // With persistent store, alice2 would be able to decrypt
    // For now, we just verify the API works
    let result = alice2.dcgka_decrypt(room_id, &ciphertext).await;

    // With MemoryStore, this will fail because state isn't persisted
    // This test demonstrates the integration point for persistence
    assert!(result.is_err(), "MemoryStore doesn't persist across restarts (expected for PoC)");
}
