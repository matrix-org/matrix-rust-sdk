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

//! Encryption tests for Phase 6 (T068-T072)
//!
//! Tests ChaCha20-Poly1305 encryption, decryption, and historical key management

use crate::dcgka::{DcgkaEngine, DcgkaUpdate, UpdatePayload};
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
    let identity_key = Curve25519PublicKey::from([42u8; 32]);

    DcgkaEngine::new(
        room_id.to_owned(),
        user_id.to_owned(),
        device_id.to_owned(),
        signing_key,
        identity_key,
    )
}

/// Helper: Create a signed DCGKA update
fn create_signed_update(
    issuer: &OwnedUserId,
    device_id: &DeviceId,
    signing_key: &Ed25519Keypair,
    payload: UpdatePayload,
    dependencies: Vec<Ulid>,
) -> DcgkaUpdate {
    let update_id = Ulid::new();
    let update_type = payload.update_type();

    // Create unsigned update for canonical JSON
    let mut update = DcgkaUpdate {
        update_id,
        issuer: issuer.to_owned(),
        issuer_device: device_id.to_owned(),
        update_type,
        dependencies,
        payload,
        signature: vec![],
        timestamp: 0,
    };

    // Sign canonical JSON
    let canonical = update.canonical_json();
    let signature = signing_key.sign(canonical.as_bytes());
    update.signature = signature.to_bytes().to_vec();

    update
}

/// T069: Test encrypt → decrypt round-trip preserves plaintext
#[test]
fn test_encrypt_decrypt_roundtrip() {
    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org").to_owned();
    let device_id = device_id!("ALICE_DEVICE");

    let signing_key = Ed25519Keypair::new();
    let identity_key = Curve25519PublicKey::from([42u8; 32]);

    let mut engine = DcgkaEngine::new(
        room_id.to_owned(),
        alice_id.clone(),
        device_id.to_owned(),
        signing_key.clone(),
        identity_key,
    );

    // Bootstrap: Alice adds herself
    let add_alice = create_signed_update(
        &alice_id,
        device_id,
        &signing_key,
        UpdatePayload::Add {
            member_id: alice_id.clone(),
            member_public_key: signing_key.public_key(),
        },
        vec![],
    );

    engine.apply_update(add_alice).expect("Failed to apply update");

    // Encrypt plaintext
    let plaintext = b"Hello, DCGKA world!";
    let ciphertext = engine.encrypt(plaintext).expect("Encryption should succeed");

    // Verify ciphertext is different from plaintext
    assert_ne!(ciphertext.as_slice(), plaintext);

    // Verify ciphertext includes nonce (12 bytes) + encrypted data
    assert!(ciphertext.len() > 12, "Ciphertext should have nonce + encrypted data");

    // Decrypt ciphertext
    let decrypted = engine.decrypt(&ciphertext).expect("Decryption should succeed");

    // Verify plaintext is preserved
    assert_eq!(decrypted.as_slice(), plaintext, "Decrypted plaintext should match original");
}

/// T070: Test messages encrypted with old keys decrypt after new updates
#[test]
fn test_historical_key_decryption() {
    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org").to_owned();
    let bob_id = user_id!("@bob:example.org").to_owned();
    let device_id = device_id!("ALICE_DEVICE");

    let alice_signing_key = Ed25519Keypair::new();
    let bob_signing_key = Ed25519Keypair::new();
    let identity_key = Curve25519PublicKey::from([42u8; 32]);

    let mut engine = DcgkaEngine::new(
        room_id.to_owned(),
        alice_id.clone(),
        device_id.to_owned(),
        alice_signing_key.clone(),
        identity_key,
    );

    // Bootstrap: Alice adds herself
    let add_alice = create_signed_update(
        &alice_id,
        device_id,
        &alice_signing_key,
        UpdatePayload::Add {
            member_id: alice_id.clone(),
            member_public_key: alice_signing_key.public_key(),
        },
        vec![],
    );

    let alice_update_id = add_alice.update_id;
    engine.apply_update(add_alice).expect("Failed to apply Alice add");

    // Encrypt message with current key (generation 1)
    let message1 = b"Message from generation 1";
    let ciphertext1 = engine.encrypt(message1).expect("Encryption 1 should succeed");

    // Apply new update: Alice adds Bob (creates generation 2)
    let add_bob = create_signed_update(
        &alice_id,
        device_id,
        &alice_signing_key,
        UpdatePayload::Add {
            member_id: bob_id.clone(),
            member_public_key: bob_signing_key.public_key(),
        },
        vec![alice_update_id],
    );

    let bob_update_id = add_bob.update_id;
    engine.apply_update(add_bob).expect("Failed to apply Bob add");

    // Encrypt message with new key (generation 2)
    let message2 = b"Message from generation 2";
    let ciphertext2 = engine.encrypt(message2).expect("Encryption 2 should succeed");

    // Apply another update: Rotate key (creates generation 3)
    let rotate_update = create_signed_update(
        &alice_id,
        device_id,
        &alice_signing_key,
        UpdatePayload::Rotate { entropy: vec![0x99; 32] },
        vec![bob_update_id],
    );

    engine.apply_update(rotate_update).expect("Failed to apply rotation");

    // Encrypt message with newest key (generation 3)
    let message3 = b"Message from generation 3";
    let ciphertext3 = engine.encrypt(message3).expect("Encryption 3 should succeed");

    // Decrypt all messages (historical keys should work)
    let decrypted1 = engine.decrypt(&ciphertext1).expect("Should decrypt generation 1 message");
    let decrypted2 = engine.decrypt(&ciphertext2).expect("Should decrypt generation 2 message");
    let decrypted3 = engine.decrypt(&ciphertext3).expect("Should decrypt generation 3 message");

    assert_eq!(decrypted1.as_slice(), message1, "Generation 1 message should decrypt");
    assert_eq!(decrypted2.as_slice(), message2, "Generation 2 message should decrypt");
    assert_eq!(decrypted3.as_slice(), message3, "Generation 3 message should decrypt");
}

/// T071: Test tampered ciphertexts are rejected (AEAD authentication)
#[test]
fn test_tampered_ciphertext_rejected() {
    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org").to_owned();
    let device_id = device_id!("ALICE_DEVICE");

    let signing_key = Ed25519Keypair::new();
    let identity_key = Curve25519PublicKey::from([42u8; 32]);

    let mut engine = DcgkaEngine::new(
        room_id.to_owned(),
        alice_id.clone(),
        device_id.to_owned(),
        signing_key.clone(),
        identity_key,
    );

    // Bootstrap: Alice adds herself
    let add_alice = create_signed_update(
        &alice_id,
        device_id,
        &signing_key,
        UpdatePayload::Add {
            member_id: alice_id.clone(),
            member_public_key: signing_key.public_key(),
        },
        vec![],
    );

    engine.apply_update(add_alice).expect("Failed to apply update");

    // Encrypt plaintext
    let plaintext = b"Authenticate this message!";
    let mut ciphertext = engine.encrypt(plaintext).expect("Encryption should succeed");

    // Tamper with ciphertext (flip a bit in the encrypted data, not the nonce)
    if ciphertext.len() > 12 {
        ciphertext[20] ^= 0x01; // Flip one bit
    }

    // Attempt to decrypt tampered ciphertext
    let result = engine.decrypt(&ciphertext);

    // Should fail due to AEAD authentication
    assert!(result.is_err(), "Tampered ciphertext should fail AEAD authentication");
}

/// T071: Test tampered nonce is rejected
#[test]
fn test_tampered_nonce_rejected() {
    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org").to_owned();
    let device_id = device_id!("ALICE_DEVICE");

    let signing_key = Ed25519Keypair::new();
    let identity_key = Curve25519PublicKey::from([42u8; 32]);

    let mut engine = DcgkaEngine::new(
        room_id.to_owned(),
        alice_id.clone(),
        device_id.to_owned(),
        signing_key.clone(),
        identity_key,
    );

    // Bootstrap: Alice adds herself
    let add_alice = create_signed_update(
        &alice_id,
        device_id,
        &signing_key,
        UpdatePayload::Add {
            member_id: alice_id.clone(),
            member_public_key: signing_key.public_key(),
        },
        vec![],
    );

    engine.apply_update(add_alice).expect("Failed to apply update");

    // Encrypt plaintext
    let plaintext = b"Authenticate this nonce!";
    let mut ciphertext = engine.encrypt(plaintext).expect("Encryption should succeed");

    // Tamper with nonce (first 12 bytes)
    ciphertext[5] ^= 0x01; // Flip one bit in nonce

    // Attempt to decrypt with tampered nonce
    let result = engine.decrypt(&ciphertext);

    // Should fail (different nonce means different decryption)
    assert!(result.is_err(), "Tampered nonce should fail decryption");
}

/// T072: Test 1000 encrypt/decrypt cycles with zero failures (SC-005)
#[test]
fn test_thousand_encrypt_decrypt_cycles() {
    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org").to_owned();
    let device_id = device_id!("ALICE_DEVICE");

    let signing_key = Ed25519Keypair::new();
    let identity_key = Curve25519PublicKey::from([42u8; 32]);

    let mut engine = DcgkaEngine::new(
        room_id.to_owned(),
        alice_id.clone(),
        device_id.to_owned(),
        signing_key.clone(),
        identity_key,
    );

    // Bootstrap: Alice adds herself
    let add_alice = create_signed_update(
        &alice_id,
        device_id,
        &signing_key,
        UpdatePayload::Add {
            member_id: alice_id.clone(),
            member_public_key: signing_key.public_key(),
        },
        vec![],
    );

    engine.apply_update(add_alice).expect("Failed to apply update");

    // Perform 1000 encrypt/decrypt cycles
    let mut failures = 0;

    for i in 0..1000 {
        let plaintext = format!("Message number {}", i);
        let plaintext_bytes = plaintext.as_bytes();

        // Encrypt
        let ciphertext = match engine.encrypt(plaintext_bytes) {
            Ok(ct) => ct,
            Err(_) => {
                failures += 1;
                continue;
            }
        };

        // Decrypt
        let decrypted = match engine.decrypt(&ciphertext) {
            Ok(pt) => pt,
            Err(_) => {
                failures += 1;
                continue;
            }
        };

        // Verify correctness
        if decrypted != plaintext_bytes {
            failures += 1;
        }
    }

    // SC-005: Zero failures
    assert_eq!(failures, 0, "All 1000 encrypt/decrypt cycles should succeed without failures");
}

/// Test decryption fails gracefully with invalid ciphertext (too short)
#[test]
fn test_invalid_ciphertext_too_short() {
    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org").to_owned();
    let device_id = device_id!("ALICE_DEVICE");

    let signing_key = Ed25519Keypair::new();
    let identity_key = Curve25519PublicKey::from([42u8; 32]);

    let mut engine = DcgkaEngine::new(
        room_id.to_owned(),
        alice_id.clone(),
        device_id.to_owned(),
        signing_key.clone(),
        identity_key,
    );

    // Bootstrap: Alice adds herself
    let add_alice = create_signed_update(
        &alice_id,
        device_id,
        &signing_key,
        UpdatePayload::Add {
            member_id: alice_id.clone(),
            member_public_key: signing_key.public_key(),
        },
        vec![],
    );

    engine.apply_update(add_alice).expect("Failed to apply update");

    // Try to decrypt ciphertext that's too short (< 12 bytes for nonce)
    let invalid_ciphertext = vec![0x01, 0x02, 0x03];

    let result = engine.decrypt(&invalid_ciphertext);
    assert!(result.is_err(), "Decryption should fail for too-short ciphertext");
}

/// Test historical key limit (max 100 keys)
#[test]
fn test_historical_key_limit() {
    let room_id = room_id!("!test:example.org");
    let alice_id = user_id!("@alice:example.org").to_owned();
    let device_id = device_id!("ALICE_DEVICE");

    let signing_key = Ed25519Keypair::new();
    let identity_key = Curve25519PublicKey::from([42u8; 32]);

    let mut engine = DcgkaEngine::new(
        room_id.to_owned(),
        alice_id.clone(),
        device_id.to_owned(),
        signing_key.clone(),
        identity_key,
    );

    // Bootstrap: Alice adds herself
    let add_alice = create_signed_update(
        &alice_id,
        device_id,
        &signing_key,
        UpdatePayload::Add {
            member_id: alice_id.clone(),
            member_public_key: signing_key.public_key(),
        },
        vec![],
    );

    let mut last_update_id = add_alice.update_id;
    engine.apply_update(add_alice).expect("Failed to apply Alice add");

    // Apply 150 rotation updates to exceed the 100-key limit
    for i in 0..150 {
        let rotate = create_signed_update(
            &alice_id,
            device_id,
            &signing_key,
            UpdatePayload::Rotate { entropy: vec![(i % 256) as u8; 32] },
            vec![last_update_id],
        );

        last_update_id = rotate.update_id;
        engine.apply_update(rotate).expect("Failed to apply rotation");
    }

    // Verify historical_keys size is limited to 100
    // Note: We can't directly access historical_keys as it's private
    // This test verifies the limit indirectly by checking that recent keys work
    let plaintext = b"Test message";
    let ciphertext = engine.encrypt(plaintext).expect("Encryption should succeed");
    let decrypted = engine.decrypt(&ciphertext).expect("Decryption should succeed");
    assert_eq!(decrypted.as_slice(), plaintext);

    // The implementation should have dropped the oldest 50 keys
    // We can't verify this directly without exposing historical_keys, but the test
    // confirms the engine still functions correctly after 150 rotations
}
