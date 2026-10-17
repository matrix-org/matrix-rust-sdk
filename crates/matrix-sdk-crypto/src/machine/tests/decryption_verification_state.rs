// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::{assert_matches, iter, sync::Arc};

use matrix_sdk_common::deserialized_responses::{
    DeviceLinkProblem, ShieldState, VerificationLevel, VerificationState,
};
use matrix_sdk_test::{async_test, ruma_response_from_json, test_json};
use ruma::{
    MilliSecondsSinceUnixEpoch, RoomId, TransactionId,
    events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
    room_id,
    serde::Raw,
    user_id,
};
use serde_json::json;
use strass::assert_let;
use vodozemac::{Curve25519PublicKey, Ed25519PublicKey};

use crate::{
    CryptoStoreError, DecryptionSettings, DeviceData, EncryptionSettings, LocalTrust, MegolmError,
    OlmMachine, OtherUserIdentityData, TrustRequirement, UserIdentity,
    machine::{
        test_helpers::{
            get_machine_pair_with_setup_sessions_test_helper, get_prepared_machine_test_helper,
        },
        tests,
    },
    olm::{InboundGroupSession, OutboundGroupSession, SenderData},
    store::types::{Changes, IdentityChanges},
    types::{
        CrossSigningKey, DeviceKeys, EventEncryptionAlgorithm, MasterPubkey, SelfSigningPubkey,
        events::{
            ToDeviceEvent,
            room::encrypted::{EncryptedEvent, RoomEventEncryptionScheme},
        },
        requests::AnyOutgoingRequest,
    },
    utilities::json_convert,
};

#[async_test]
async fn test_decryption_verification_state() {
    macro_rules! assert_shield {
        ($foo: ident, $strict: ident, $lax: ident) => {
            let lax = $foo.verification_state.to_shield_state_lax();
            let strict = $foo.verification_state.to_shield_state_strict();

            assert_matches!(lax, ShieldState::$lax { .. });
            assert_matches!(strict, ShieldState::$strict { .. });
        };
    }
    let (alice, bob) = get_machine_pair_with_setup_sessions_test_helper(
        tests::alice_id(),
        tests::user_id(),
        false,
    )
    .await;
    let room_id = room_id!("!test:example.org");

    let to_device_requests = alice
        .share_room_key(room_id, iter::once(bob.user_id()), EncryptionSettings::default())
        .await
        .unwrap();

    let event = ToDeviceEvent::new(
        alice.user_id().to_owned(),
        tests::to_device_requests_to_content(to_device_requests),
    );

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let group_session = bob
        .store()
        .with_transaction(async |tr| {
            let res = bob
                .decrypt_to_device_event(tr, &event, &mut Changes::default(), &decryption_settings)
                .await?;
            Ok(res)
        })
        .await
        .unwrap()
        .inbound_group_session;

    let export = group_session.as_ref().unwrap().clone().export().await;

    bob.store().save_inbound_group_sessions(&[group_session.unwrap()]).await.unwrap();

    let plaintext = "It is a secret to everybody";

    let content = RoomMessageEventContent::text_plain(plaintext);

    let result = alice
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
        .await
        .unwrap();

    let event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": alice.user_id(),
        "type": "m.room.encrypted",
        "content": result.content,
    });

    let event = json_convert(&event).unwrap();

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };
    let encryption_info = bob
        .decrypt_room_event(&event, room_id, &decryption_settings)
        .await
        .unwrap()
        .encryption_info;

    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnsignedDevice),
        encryption_info.verification_state
    );

    assert_shield!(encryption_info, Red, Red);

    // get_room_event_encryption_info should return the same information
    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();
    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnsignedDevice),
        encryption_info.verification_state
    );
    assert_shield!(encryption_info, Red, Red);

    // Local trust state has no effect
    bob.get_device(alice.user_id(), tests::alice_device_id(), None)
        .await
        .unwrap()
        .unwrap()
        .set_trust_state(LocalTrust::Verified);

    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnsignedDevice),
        encryption_info.verification_state
    );
    assert_shield!(encryption_info, Red, Red);

    tests::setup_cross_signing_for_machine_test_helper(&alice, &bob).await;
    let bob_id_from_alice = alice.get_identity(bob.user_id(), None).await.unwrap();
    assert_matches!(bob_id_from_alice, Some(UserIdentity::Other(_)));
    let alice_id_from_bob = bob.get_identity(alice.user_id(), None).await.unwrap();
    assert_matches!(alice_id_from_bob, Some(UserIdentity::Other(_)));

    // we setup cross signing but nothing is signed yet
    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnsignedDevice),
        encryption_info.verification_state
    );
    assert_shield!(encryption_info, Red, Red);

    // Let alice sign her device
    tests::sign_alice_device_for_machine_test_helper(&alice, &bob).await;

    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

    assert_eq!(
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity),
        encryption_info.verification_state
    );

    assert_shield!(encryption_info, Red, None);

    // Given alice is verified
    mark_alice_identity_as_verified_test_helper(&alice, &bob).await;

    // Update bob's store to make sure there is no useful SenderData for this
    // session.
    let mut session = load_session(&bob, room_id, &event).await.unwrap().unwrap();
    session.sender_data = SenderData::unknown();
    save_session(&bob, session).await.unwrap();

    // When I get the encryption info
    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();
    // Then it should say Verified
    assert_eq!(VerificationState::Verified, encryption_info.verification_state);
    assert_shield!(encryption_info, None, None);

    // And the updated SenderData should have been saved into the store.
    let session = load_session(&bob, room_id, &event).await.unwrap().unwrap();
    assert_let!(SenderData::SenderVerified { .. } = session.sender_data);

    // Simulate an imported session, to change verification state
    let imported = InboundGroupSession::from_export(&export).unwrap();
    bob.store().save_inbound_group_sessions(&[imported]).await.unwrap();

    let encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();

    // As soon as the key source is unsafe the verification state (or existence)
    // of the device is meaningless
    assert_eq!(
        VerificationState::Unverified(VerificationLevel::None(DeviceLinkProblem::InsecureSource)),
        encryption_info.verification_state
    );

    assert_shield!(encryption_info, Red, Grey);
}

async fn load_session(
    machine: &OlmMachine,
    room_id: &RoomId,
    event: &Raw<EncryptedEvent>,
) -> Result<Option<InboundGroupSession>, CryptoStoreError> {
    let event = event.deserialize().unwrap();
    let session_id = match &event.content.scheme {
        RoomEventEncryptionScheme::MegolmV1AesSha2(s) => &s.session_id,
        #[cfg(feature = "experimental-algorithms")]
        RoomEventEncryptionScheme::MegolmV2AesSha2(s) => &s.session_id,
        RoomEventEncryptionScheme::Unknown(_) => {
            panic!("Unknown encryption scheme - can't find session ID!")
        }
    };

    machine.store().get_inbound_group_session(room_id, session_id).await
}

async fn save_session(
    machine: &OlmMachine,
    session: InboundGroupSession,
) -> Result<(), CryptoStoreError> {
    machine.store().save_inbound_group_sessions(&[session]).await
}

pub async fn mark_alice_identity_as_verified_test_helper(alice: &OlmMachine, bob: &OlmMachine) {
    let alice_device =
        bob.get_device(alice.user_id(), alice.device_id(), None).await.unwrap().unwrap();

    let alice_identity =
        bob.get_identity(alice.user_id(), None).await.unwrap().unwrap().other().unwrap();
    let upload_request = alice_identity.verify().await.unwrap();

    let raw_extracted =
        upload_request.signed_keys.get(alice.user_id()).unwrap().iter().next().unwrap().1.get();

    let new_signature: CrossSigningKey = serde_json::from_str(raw_extracted).unwrap();

    let user_key_id = bob
        .bootstrap_cross_signing(false)
        .await
        .expect("Expect Alice x-signing key request")
        .upload_signing_keys_req
        .user_signing_key
        .unwrap()
        .get_first_key_and_id()
        .unwrap()
        .0
        .to_owned();

    // add the new signature to alice msk
    let mut alice_updated_msk =
        alice_device.device_owner_identity.as_ref().unwrap().master_key().as_ref().to_owned();

    alice_updated_msk.signatures.add_signature(
        bob.user_id().to_owned(),
        user_key_id.to_owned(),
        new_signature.signatures.get_signature(bob.user_id(), &user_key_id).unwrap(),
    );

    let alice_x_keys = alice
        .bootstrap_cross_signing(false)
        .await
        .expect("Expect Alice x-signing key request")
        .upload_signing_keys_req;

    let json = json!({
        "device_keys": {
            alice.user_id() : { alice.device_id():  alice_device.as_device_keys().to_owned() }
        },
        "failures": {},
        "master_keys": {
            alice.user_id() : alice_updated_msk,
        },
        "user_signing_keys": {
            alice.user_id() : alice_x_keys.user_signing_key.unwrap(),
        },
        "self_signing_keys": {
            alice.user_id() : alice_x_keys.self_signing_key.unwrap(),
        },
      }
    );

    let kq_response = ruma_response_from_json(&json);
    alice.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();
    bob.receive_keys_query_response(&TransactionId::new(), &kq_response).await.unwrap();

    // so alice identity should be now trusted

    assert!(
        bob.get_identity(alice.user_id(), None)
            .await
            .unwrap()
            .unwrap()
            .other()
            .unwrap()
            .is_verified()
    );
}

#[async_test]
async fn test_verification_states_spoofed_sender_untrusted() {
    test_verification_states_spoofed_sender(TrustRequirement::Untrusted).await;
}

#[async_test]
async fn test_verification_states_spoofed_sender_cross_signed() {
    test_verification_states_spoofed_sender(TrustRequirement::CrossSigned).await;
}

/// Test that the verification state is set correctly when the sender of an
/// event does not match the owner of the device that sent us the session.
///
/// In this test, Bob receives an event from Alice, but the HS admin has
/// rewritten the `sender` of the event to look like another user.
///
/// We run this test a couple of times, with different [`TrustRequirement`]s.
async fn test_verification_states_spoofed_sender(
    sender_device_trust_requirement: TrustRequirement,
) {
    let (alice, bob) = get_machine_pair_with_setup_sessions_test_helper(
        tests::alice_id(),
        tests::user_id(),
        false,
    )
    .await;
    bob.bootstrap_cross_signing(false).await.unwrap();
    set_up_alice_cross_signing(&alice, &bob).await;

    let room_id = room_id!("!test:example.org");
    let decryption_settings = DecryptionSettings { sender_device_trust_requirement };

    // Alice sends a message to Bob.
    let (event, _) = encrypt_message(&alice, room_id, &bob, "Secret message").await;
    bob.decrypt_room_event(&event, room_id, &decryption_settings)
        .await
        .expect("Bob could not decrypt event");
    let event_encryption_info = bob.get_room_event_encryption_info(&event, room_id).await.unwrap();
    assert_matches!(
        &event_encryption_info.verification_state,
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
    );

    // Alice now sends a second message to Bob, using the same room key, but the
    // HS admin rewrites the 'sender' to Charlie.
    let result = alice
        .encrypt_room_event(
            room_id,
            AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent::text_plain(
                "spoofed message",
            )),
        )
        .await
        .unwrap();
    let event = json!({
        "event_id": "$xxxxy:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": "@charlie:example.org",  // Note! spoofed sender
        "type": "m.room.encrypted",
        "content": result.content,
    });
    let event = json_convert(&event).unwrap();

    let decryption_result = bob.decrypt_room_event(&event, room_id, &decryption_settings).await;

    if matches!(sender_device_trust_requirement, TrustRequirement::Untrusted) {
        // In "Untrusted" mode, the event is decrypted correctly, but the
        // verification_state should be `MismatchedSender`.
        decryption_result.expect("Bob could not decrypt spoofed event");

        let event_encryption_info =
            bob.get_room_event_encryption_info(&event, room_id).await.unwrap();
        assert_matches!(
            &event_encryption_info.verification_state,
            VerificationState::Unverified(VerificationLevel::MismatchedSender)
        );
    } else {
        // In "CrossSigned" mode, we refuse to decrypt the event altogether.
        let err =
            decryption_result.expect_err("Bob was unexpectedly able to decrypt spoofed event");
        assert_matches!(
            err,
            MegolmError::SenderIdentityNotTrusted(VerificationLevel::MismatchedSender)
        );
    }
}

#[async_test]
async fn test_verification_states_multiple_device() {
    let (bob, _) = get_prepared_machine_test_helper(tests::user_id(), false).await;

    let other_user_id = user_id!("@web2:localhost:8482");

    let json = &test_json::KEYS_QUERY_TWO_DEVICES_ONE_SIGNED;
    let response = ruma_response_from_json(json);

    let (device_change, identity_change) =
        bob.receive_keys_query_response(&TransactionId::new(), &response).await.unwrap();
    assert_eq!(device_change.new.len(), 2);
    assert_eq!(identity_change.new.len(), 1);
    //
    let devices = bob.store().get_user_devices(other_user_id).await.unwrap();
    assert_eq!(devices.devices().count(), 2);

    let fake_room_id = room_id!("!roomid:example.com");

    // We just need a fake session to export it We will use the export to create
    // various inbounds with other claimed ownership
    let id_keys = bob.identity_keys();
    let fake_device_id = bob.device_id().into();
    let olm = OutboundGroupSession::new(
        fake_device_id,
        Arc::new(id_keys),
        fake_room_id,
        EncryptionSettings::default(),
    )
    .unwrap()
    .session_key()
    .await;

    let web_unverified_inbound_session = InboundGroupSession::new(
        Curve25519PublicKey::from_base64("LTpv2DGMhggPAXO02+7f68CNEp6A40F0Yl8B094Y8gc").unwrap(),
        Ed25519PublicKey::from_base64("loz5i40dP+azDtWvsD0L/xpnCjNkmrcvtXVXzCHX8Vw").unwrap(),
        fake_room_id,
        &olm,
        SenderData::unknown(),
        None,
        EventEncryptionAlgorithm::MegolmV1AesSha2,
        None,
        false,
    )
    .unwrap();

    let (state, _) = bob
        .get_room_event_verification_state(&web_unverified_inbound_session, other_user_id)
        .await
        .unwrap();
    assert_eq!(VerificationState::Unverified(VerificationLevel::UnsignedDevice), state);

    let web_signed_inbound_session = InboundGroupSession::new(
        Curve25519PublicKey::from_base64("XJixbpnfIk+RqcK5T6moqVY9d9Q1veR8WjjSlNiQNT0").unwrap(),
        Ed25519PublicKey::from_base64("48f3WQAMGwYLBg5M5qUhqnEVA8yeibjZpPsShoWMFT8").unwrap(),
        fake_room_id,
        &olm,
        SenderData::unknown(),
        None,
        EventEncryptionAlgorithm::MegolmV1AesSha2,
        None,
        false,
    )
    .unwrap();

    let (state, _) = bob
        .get_room_event_verification_state(&web_signed_inbound_session, other_user_id)
        .await
        .unwrap();

    assert_eq!(VerificationState::Unverified(VerificationLevel::UnverifiedIdentity), state);
}

/// Test that the trust requirement is checked when decrypting an event.
///
/// Set the sender data to various values, and test that we can or can't
/// decrypt, depending on what the trust requirement is.
#[async_test]
async fn test_decryption_trust_requirement() {
    let (alice, bob) = get_machine_pair_with_setup_sessions_test_helper(
        tests::alice_id(),
        tests::user_id(),
        false,
    )
    .await;
    let room_id = room_id!("!test:example.org");
    let (event, session_id) = encrypt_message(&alice, room_id, &bob, "Secret message").await;

    // Set the SenderData on the megolm session used to encrypt `event` to
    // `DeviceInfo` (ie, we have the device keys but no cross-signing
    // information). Events sent on such a session should be decryptable only
    // when the trust requirement allows untrusted or legacy sessions.
    let mut session =
        bob.store().get_inbound_group_session(room_id, &session_id).await.unwrap().unwrap();
    session.sender_data = SenderData::DeviceInfo {
        device_keys: alice
            .get_device(alice.user_id(), alice.device_id(), None)
            .await
            .unwrap()
            .unwrap()
            .as_device_keys()
            .clone(),
        legacy_session: false,
    };
    bob.store().save_inbound_group_sessions(&[session.clone()]).await.unwrap();

    check_decryption_trust_requirement(
        &bob,
        &event,
        room_id,
        &[
            (TrustRequirement::Untrusted, true),
            (TrustRequirement::CrossSignedOrLegacy, false),
            (TrustRequirement::CrossSigned, false),
        ],
    )
    .await;

    // Bob later gets Alice's identity and cross-signed device keys
    bob.bootstrap_cross_signing(false).await.unwrap();
    set_up_alice_cross_signing(&alice, &bob).await;

    // Since the sending device is now cross-signed by Alice, it should be
    // decryptable in all modes
    check_decryption_trust_requirement(
        &bob,
        &event,
        room_id,
        &[
            (TrustRequirement::Untrusted, true),
            (TrustRequirement::CrossSignedOrLegacy, true),
            (TrustRequirement::CrossSigned, true),
        ],
    )
    .await;
}

/// Test that the trust requirement is correctly handled when a user's
/// cross-signing identity changes.
#[async_test]
async fn test_decryption_trust_with_identity_change() {
    let (alice, bob) = get_machine_pair_with_setup_sessions_test_helper(
        tests::alice_id(),
        tests::user_id(),
        false,
    )
    .await;
    bob.bootstrap_cross_signing(false).await.unwrap();
    let room_id = room_id!("!test:example.org");
    let (event, session_id) = encrypt_message(&alice, room_id, &bob, "Secret message").await;
    set_up_alice_cross_signing(&alice, &bob).await;

    // Simulate Alice's cross-signing key changing after having been verified by
    // setting the `previously_verified` flag
    let alice_identity =
        bob.store().get_identity(alice.user_id()).await.unwrap().unwrap().other().unwrap();
    alice_identity.mark_as_previously_verified().await.unwrap();

    // Bob receives the Megolm session and message
    let mut session =
        bob.store().get_inbound_group_session(room_id, &session_id).await.unwrap().unwrap();
    session.sender_data =
        SenderData::UnknownDevice { legacy_session: false, owner_check_failed: false };
    bob.store().save_inbound_group_sessions(&[session.clone()]).await.unwrap();

    // In this case, the message should not be decryptable except for when we
    // accept untrusted devices
    check_decryption_trust_requirement(
        &bob,
        &event,
        room_id,
        &[
            (TrustRequirement::Untrusted, true),
            (TrustRequirement::CrossSignedOrLegacy, false),
            (TrustRequirement::CrossSigned, false),
        ],
    )
    .await;
}

/// A session imported from backup is a legacy session, so it should stay
/// decryptable under `CrossSignedOrLegacy` once we learn about the sender's
/// device via `/keys/query`.
#[async_test]
async fn test_backup_imported_session_stays_legacy_after_keys_query() {
    // Alice sends an encrypted message to Bob's first device
    let (alice, bob) = get_machine_pair_with_setup_sessions_test_helper(
        tests::alice_id(),
        tests::user_id(),
        false,
    )
    .await;
    let room_id = room_id!("!test:example.org");
    let (event, session_id) = encrypt_message(&alice, room_id, &bob, "Secret message").await;
    let exported = bob
        .store()
        .get_inbound_group_session(room_id, &session_id)
        .await
        .unwrap()
        .unwrap()
        .export()
        .await;

    // A fresh device of Bob's restores the room key from backup, knowing
    // nothing about Alice's devices. We simulate a restore from backup via
    // `import_room_keys`.
    let bob2 = OlmMachine::new(tests::user_id(), ruma::device_id!("BOB2")).await;
    bob2.store().import_room_keys(vec![exported], Some("1"), |_, _| {}).await.unwrap();
    check_decryption_trust_requirement(
        &bob2,
        &event,
        room_id,
        &[(TrustRequirement::CrossSignedOrLegacy, true)],
    )
    .await;

    // Bob's new device then gets Alice's (not cross-signed) device in a
    // `/keys/query` response.
    let alice_device = DeviceData::from_machine_test_helper(&alice).await.unwrap();
    let kq_response = json!({
        "device_keys": { alice.user_id(): { alice.device_id(): alice_device.as_device_keys() } }
    });
    bob2.receive_keys_query_response(&TransactionId::new(), &ruma_response_from_json(&kq_response))
        .await
        .unwrap();

    // Check that Bob's new device can still successfully decrypt the event.
    check_decryption_trust_requirement(
        &bob2,
        &event,
        room_id,
        &[(TrustRequirement::CrossSignedOrLegacy, true)],
    )
    .await;
}

/// Helper function to set up Alice's cross-signing, and save her keys in Bob's
/// storage.
async fn set_up_alice_cross_signing(alice: &OlmMachine, bob: &OlmMachine) {
    let cross_signing_requests = alice.bootstrap_cross_signing(false).await.unwrap();
    let upload_signing_keys_req = cross_signing_requests.upload_signing_keys_req;
    let alice_msk: MasterPubkey = upload_signing_keys_req.master_key.unwrap().try_into().unwrap();
    let alice_ssk: SelfSigningPubkey =
        upload_signing_keys_req.self_signing_key.unwrap().try_into().unwrap();
    let upload_keys_req = cross_signing_requests.upload_keys_req.unwrap().clone();
    assert_let!(
        AnyOutgoingRequest::KeysUpload(device_upload_request) = upload_keys_req.request.as_ref()
    );
    bob.store()
        .save_device_data(&[DeviceData::try_from(
            &device_upload_request
                .device_keys
                .as_ref()
                .unwrap()
                .deserialize_as::<DeviceKeys>()
                .unwrap(),
        )
        .unwrap()])
        .await
        .unwrap();
    bob.store()
        .save_changes(Changes {
            identities: IdentityChanges {
                new: vec![
                    OtherUserIdentityData::new(alice_msk.clone(), alice_ssk.clone())
                        .unwrap()
                        .into(),
                ],
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
}

/// Helper function that encrypts a message and shares the Megolm session with a
/// recipient.
async fn encrypt_message(
    sender: &OlmMachine,
    room_id: &RoomId,
    recipient: &OlmMachine,
    plaintext: &str,
) -> (Raw<EncryptedEvent>, String) {
    let to_device_requests = sender
        .share_room_key(room_id, iter::once(recipient.user_id()), EncryptionSettings::default())
        .await
        .unwrap();

    let event = ToDeviceEvent::new(
        sender.user_id().to_owned(),
        tests::to_device_requests_to_content(to_device_requests),
    );

    let decryption_settings =
        DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

    let group_session = recipient
        .store()
        .with_transaction(async |tr| {
            let res = recipient
                .decrypt_to_device_event(tr, &event, &mut Changes::default(), &decryption_settings)
                .await?;
            Ok(res)
        })
        .await
        .unwrap()
        .inbound_group_session
        .unwrap();
    let sessions = std::slice::from_ref(&group_session);
    recipient.store().save_inbound_group_sessions(sessions).await.unwrap();

    let content = RoomMessageEventContent::text_plain(plaintext);

    let result = sender
        .encrypt_room_event(room_id, AnyMessageLikeEventContent::RoomMessage(content.clone()))
        .await
        .unwrap();

    let event = json!({
        "event_id": "$xxxxx:example.org",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
        "sender": sender.user_id(),
        "type": "m.room.encrypted",
        "content": result.content,
    });
    let event = json_convert(&event).unwrap();

    (event, group_session.session_id().to_owned())
}

/// Helper function that checks whether a message is decryptable under different
/// trust requirements.
///
/// `tests` is a list of tuples, where the first element of the tuple is the
/// trust requirement to check, and the second element indicates whether
/// decryption should succeed (`true`) or fail (`false`).
async fn check_decryption_trust_requirement(
    bob: &OlmMachine,
    event: &Raw<EncryptedEvent>,
    room_id: &RoomId,
    tests: &[(TrustRequirement, bool)],
) {
    for (trust_requirement, is_ok) in tests {
        let decryption_settings =
            DecryptionSettings { sender_device_trust_requirement: *trust_requirement };
        if *is_ok {
            assert!(
                bob.decrypt_room_event(event, room_id, &decryption_settings).await.is_ok(),
                "Decryption did not succeed with {trust_requirement:?}",
            );
        } else {
            assert!(
                bob.decrypt_room_event(event, room_id, &decryption_settings).await.is_err(),
                "Decryption succeeded with {trust_requirement:?}",
            );
        }
    }
}

/// Tests for MSC4350: a message sent with a bridge bot's device on behalf of a
/// ghost user is only trusted when the ghost has a valid impersonatable device
/// and the bot's device is cross-signed.
#[cfg(feature = "experimental-msc4350")]
mod impersonation {
    use ruma::{CanonicalJsonValue, UserId, device_id};
    use serde_json::Value;

    use super::*;
    use crate::machine::CrossSigningBootstrapRequests;

    const MISMATCHED: VerificationState =
        VerificationState::Unverified(VerificationLevel::MismatchedSender);
    const TRUSTED_UNVERIFIED: VerificationState =
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity);

    fn ghost_id() -> &'static UserId {
        user_id!("@ghost:example.org")
    }

    /// A bridge bot ("alice") that has shared a room key with a user ("bob")
    /// and sent a message, which can be presented as coming from any sender.
    struct Fixture {
        bot: OlmMachine,
        bob: OlmMachine,
        content: Value,
        settings: DecryptionSettings,
    }

    impl Fixture {
        const ROOM_ID: &'static str = "!test:example.org";

        fn room_id() -> &'static RoomId {
            room_id!("!test:example.org")
        }

        async fn new() -> Self {
            let (bot, bob) = get_machine_pair_with_setup_sessions_test_helper(
                tests::alice_id(),
                tests::user_id(),
                false,
            )
            .await;
            let settings =
                DecryptionSettings { sender_device_trust_requirement: TrustRequirement::Untrusted };

            let to_device_requests = bot
                .share_room_key(
                    Self::room_id(),
                    iter::once(bob.user_id()),
                    EncryptionSettings::default(),
                )
                .await
                .unwrap();
            let event = ToDeviceEvent::new(
                bot.user_id().to_owned(),
                tests::to_device_requests_to_content(to_device_requests),
            );
            let group_session = bob
                .store()
                .with_transaction(async |tr| {
                    Ok(bob
                        .decrypt_to_device_event(tr, &event, &mut Changes::default(), &settings)
                        .await?)
                })
                .await
                .unwrap()
                .inbound_group_session
                .unwrap();
            bob.store().save_inbound_group_sessions(&[group_session]).await.unwrap();

            let encrypted = bot
                .encrypt_room_event(
                    Self::room_id(),
                    AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent::text_plain(
                        "hello from the ghost",
                    )),
                )
                .await
                .unwrap();
            let content = serde_json::to_value(&encrypted.content).unwrap();
            // Impersonation is only accepted if the membership of the
            // impersonator can be checked, so by default everyone
            // is in the room.
            bob.set_impersonator_membership_check(Some(Arc::new(|_, _| Box::pin(async { true }))));
            Self { bot, bob, content, settings }
        }

        /// The encrypted message, presented as sent by `sender`.
        fn event(&self, sender: &UserId) -> Raw<EncryptedEvent> {
            json_convert(&json!({
                "event_id": "$xxxxx:example.org",
                "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
                "sender": sender,
                "type": "m.room.encrypted",
                "content": self.content,
            }))
            .unwrap()
        }

        async fn state(&self, sender: &UserId) -> VerificationState {
            self.bob
                .decrypt_room_event(&self.event(sender), Self::room_id(), &self.settings)
                .await
                .unwrap()
                .encryption_info
                .verification_state
                .clone()
        }

        async fn cross_sign_bot(&self) {
            tests::setup_cross_signing_for_machine_test_helper(&self.bot, &self.bob).await;
            tests::sign_alice_device_for_machine_test_helper(&self.bot, &self.bob).await;
        }

        /// The impersonatable device `user_id` would publish for the bot's
        /// device. `tweak` can change the object before the bot signs
        /// it.
        async fn ghost_device_with(
            &self,
            user_id: &UserId,
            tweak: impl FnOnce(&mut Value),
        ) -> Value {
            let device_id = self.bot.device_id().to_owned();
            let keys = self.bot.identity_keys();
            let mut device = json!({
                "user_id": user_id,
                "device_id": device_id,
                "algorithms": [],
                "keys": {},
                "fi.mau.msc4350.impersonator": {
                    "user_id": self.bot.user_id(),
                    "device_id": device_id,
                    "algorithms": ["m.olm.v1.curve25519-aes-sha2", "m.megolm.v1.aes-sha2"],
                    "keys": {
                        format!("curve25519:{device_id}"): keys.curve25519.to_base64(),
                        format!("ed25519:{device_id}"): keys.ed25519.to_base64(),
                    },
                },
            });
            tweak(&mut device);
            let signature = {
                let cache = self.bot.store().cache().await.unwrap();
                let account = cache.account().await.unwrap();
                account.sign_json(CanonicalJsonValue::try_from(device.clone()).unwrap()).unwrap()
            };
            device["signatures"] = json!({
                self.bot.user_id(): { format!("ed25519:{device_id}"): signature.to_base64() },
            });
            device
        }

        async fn ghost_device(&self, user_id: &UserId) -> Value {
            self.ghost_device_with(user_id, |_| {}).await
        }

        /// Make the user's homeserver answer a `/keys/query` with these devices
        /// (and nothing else).
        async fn receive_devices(&self, devices: Vec<(&UserId, Value)>) {
            let device_id = self.bot.device_id().to_string();
            let device_keys: serde_json::Map<String, Value> = devices
                .into_iter()
                .map(|(user_id, device)| {
                    (user_id.to_string(), json!({ device_id.clone(): device }))
                })
                .collect();
            let response = ruma_response_from_json(&json!({ "device_keys": device_keys }));
            self.bob.receive_keys_query_response(&TransactionId::new(), &response).await.unwrap();
        }

        /// Publish the ghost's device together with a cross-signing identity of
        /// the ghost. Returns the ghost's own machine, to sign things
        /// with its self-signing key.
        async fn receive_device_and_identity(&self, device: Value) -> OlmMachine {
            let ghost = OlmMachine::new(ghost_id(), device_id!("GHOSTDEVICE")).await;
            let CrossSigningBootstrapRequests { upload_signing_keys_req: keys, .. } =
                ghost.bootstrap_cross_signing(false).await.unwrap();
            let device_id = self.bot.device_id().to_string();
            let response = ruma_response_from_json(&json!({
                "device_keys": { ghost_id(): { device_id: device } },
                "master_keys": { ghost_id(): keys.master_key.unwrap() },
                "self_signing_keys": { ghost_id(): keys.self_signing_key.unwrap() },
            }));
            self.bob.receive_keys_query_response(&TransactionId::new(), &response).await.unwrap();
            ghost
        }
    }

    /// Prints a ghost device, signed by a bot device, together with the bot's
    /// device keys, as JSON. It is a test vector for other implementations
    /// (a verifier in another language should accept the bot's signature on
    /// the ghost device): `cargo test -p matrix-sdk-crypto --features
    /// experimental-msc4350 print_test_vector -- --ignored --nocapture`.
    #[async_test]
    #[ignore = "only prints a test vector"]
    async fn test_print_test_vector() {
        let f = Fixture::new().await;
        let bot_device =
            f.bot.get_device(f.bot.user_id(), f.bot.device_id(), None).await.unwrap().unwrap();
        let vector = json!({
            "description": "MSC4350 impersonatable device signed by a matrix-rust-sdk device",
            "bot_user_id": f.bot.user_id(),
            "bot_device_id": f.bot.device_id(),
            "bot_device_keys": bot_device.as_device_keys(),
            "ghost_user_id": ghost_id(),
            "ghost_device": f.ghost_device(ghost_id()).await,
        });
        println!("VECTOR_BEGIN\n{}\nVECTOR_END", serde_json::to_string_pretty(&vector).unwrap());
    }

    #[async_test]
    async fn test_message_from_unknown_sender_is_mismatched() {
        let f = Fixture::new().await;
        f.cross_sign_bot().await;
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);
    }

    #[async_test]
    async fn test_impersonated_message_is_trusted_once_the_bot_is_cross_signed() {
        let f = Fixture::new().await;
        f.receive_devices(vec![(ghost_id(), f.ghost_device(ghost_id()).await)]).await;

        // The bot's device isn't cross-signed yet.
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);

        f.cross_sign_bot().await;
        assert_eq!(TRUSTED_UNVERIFIED, f.state(ghost_id()).await);

        let info = f
            .bob
            .get_room_event_encryption_info(&f.event(ghost_id()), Fixture::room_id())
            .await
            .unwrap();
        assert_eq!(info.sender_device.as_deref(), Some(f.bot.device_id()));
        assert_matches!(info.verification_state.to_shield_state_lax(), ShieldState::None);

        // Another user the bot has no device for is still not trusted.
        assert_eq!(MISMATCHED, f.state(user_id!("@other:example.org")).await);
    }

    #[async_test]
    async fn test_tampered_impersonator_is_rejected() {
        let f = Fixture::new().await;
        f.cross_sign_bot().await;
        let mut device = f.ghost_device(ghost_id()).await;
        device["fi.mau.msc4350.impersonator"]["algorithms"] = json!(["something.else"]);
        f.receive_devices(vec![(ghost_id(), device)]).await;
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);
    }

    #[async_test]
    async fn test_impersonator_with_other_keys_is_rejected() {
        // Correctly signed, but the embedded keys aren't the bot device's real
        // keys.
        let f = Fixture::new().await;
        f.cross_sign_bot().await;
        let device_id = f.bot.device_id().to_string();
        let other_key = f.bob.identity_keys().curve25519.to_base64();
        let device = f
            .ghost_device_with(ghost_id(), |device| {
                device["fi.mau.msc4350.impersonator"]["keys"][format!("curve25519:{device_id}")] =
                    json!(other_key);
            })
            .await;
        f.receive_devices(vec![(ghost_id(), device)]).await;
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);
    }

    #[async_test]
    async fn test_device_with_its_own_keys_or_algorithms_is_rejected() {
        let f = Fixture::new().await;
        f.cross_sign_bot().await;
        let device_id = f.bot.device_id().to_string();

        let with_keys = f
            .ghost_device_with(ghost_id(), |device| {
                device["keys"] = json!({ format!("ed25519:{device_id}"): f.bot.identity_keys().ed25519.to_base64() });
            })
            .await;
        f.receive_devices(vec![(ghost_id(), with_keys)]).await;
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);

        let with_algorithms = f
            .ghost_device_with(ghost_id(), |device| {
                device["algorithms"] = json!(["m.megolm.v1.aes-sha2"]);
            })
            .await;
        f.receive_devices(vec![(ghost_id(), with_algorithms)]).await;
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);
    }

    #[async_test]
    async fn test_malformed_impersonator_is_rejected() {
        let f = Fixture::new().await;
        f.cross_sign_bot().await;
        let device = f
            .ghost_device_with(ghost_id(), |device| {
                device["fi.mau.msc4350.impersonator"] = json!("not an object");
            })
            .await;
        f.receive_devices(vec![(ghost_id(), device)]).await;
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);
    }

    #[async_test]
    async fn test_impersonator_is_replaced_and_removed_by_later_key_queries() {
        let f = Fixture::new().await;
        f.cross_sign_bot().await;
        f.receive_devices(vec![(ghost_id(), f.ghost_device(ghost_id()).await)]).await;
        assert_eq!(TRUSTED_UNVERIFIED, f.state(ghost_id()).await);

        // A later response with a broken device replaces the good one...
        let mut broken = f.ghost_device(ghost_id()).await;
        broken["fi.mau.msc4350.impersonator"]["user_id"] = json!("@mallory:example.org");
        f.receive_devices(vec![(ghost_id(), broken)]).await;
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);

        // ...and a response listing no devices removes it.
        f.receive_devices(vec![(ghost_id(), f.ghost_device(ghost_id()).await)]).await;
        assert_eq!(TRUSTED_UNVERIFIED, f.state(ghost_id()).await);
        let response = ruma_response_from_json(&json!({ "device_keys": { ghost_id(): {} } }));
        f.bob.receive_keys_query_response(&TransactionId::new(), &response).await.unwrap();
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);
    }

    #[async_test]
    async fn test_ghost_with_cross_signing_keys_must_sign_the_device() {
        let f = Fixture::new().await;
        f.cross_sign_bot().await;

        // The ghost has cross-signing keys, but its self-signing key hasn't
        // signed the impersonatable device.
        let unsigned = f.ghost_device(ghost_id()).await;
        let ghost = f.receive_device_and_identity(unsigned.clone()).await;
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);

        // Once the self-signing key has signed it, the message is trusted.
        let mut device_keys: DeviceKeys = serde_json::from_value(unsigned).unwrap();
        ghost.inner.user_identity.lock().await.sign_device_keys(&mut device_keys).await.unwrap();
        f.receive_devices(vec![(ghost_id(), serde_json::to_value(&device_keys).unwrap())]).await;
        assert_eq!(TRUSTED_UNVERIFIED, f.state(ghost_id()).await);
    }

    #[async_test]
    async fn test_membership_check_of_the_impersonator() {
        let f = Fixture::new().await;
        f.cross_sign_bot().await;
        f.receive_devices(vec![(ghost_id(), f.ghost_device(ghost_id()).await)]).await;
        assert_eq!(TRUSTED_UNVERIFIED, f.state(ghost_id()).await);

        let bot_id = f.bot.user_id().to_owned();
        let expected_room = Fixture::ROOM_ID.to_owned();
        f.bob.set_impersonator_membership_check(Some(Arc::new(move |room_id, user_id| {
            // Only the bot, and only in the room of the message, counts as a
            // member.
            let is_member = user_id == bot_id && room_id.as_str() == expected_room;
            Box::pin(async move { is_member })
        })));
        assert_eq!(TRUSTED_UNVERIFIED, f.state(ghost_id()).await);

        f.bob.set_impersonator_membership_check(Some(Arc::new(|_, _| Box::pin(async { false }))));
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);

        // Without a check, the membership can't be proven, so the impersonation
        // is rejected.
        f.bob.set_impersonator_membership_check(None);
        assert_eq!(MISMATCHED, f.state(ghost_id()).await);
    }
}
