use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Result;
use assert_matches::assert_matches;
use assert_matches2::assert_let;
use assign::assign;
use matrix_sdk::{
    Client, assert_next_eq_with_timeout,
    encryption::{
        BackupDownloadStrategy, EncryptionSettings, LocalTrust,
        backups::BackupState,
        recovery::{Recovery, RecoveryState},
        verification::{
            QrVerificationData, QrVerificationState, Verification, VerificationRequestState,
        },
    },
    ruma::{
        OwnedEventId, OwnedRoomId,
        api::client::room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        events::{
            GlobalAccountDataEventType, OriginalSyncMessageLikeEvent,
            key::verification::{VerificationMethod, request::ToDeviceKeyVerificationRequestEvent},
            room::message::{
                MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
                SyncRoomMessageEvent,
            },
            secret_storage::secret::SecretEventContent,
        },
    },
    timeout::timeout,
};
use matrix_sdk_base::crypto::{SasState, format_emojis};
use matrix_sdk_ui::{
    notification_client::{NotificationClient, NotificationProcessSetup},
    sync_service::SyncService,
};
use similar_asserts::assert_eq;
use tracing::{debug, warn};

use crate::helpers::{SyncTokenAwareClient, TestClientBuilder};

mod shared_history;
#[cfg(feature = "experimental-encrypted-state-events")]
mod state_events;

// This test reproduces a bug seen on clients that use the same `Client`
// instance for both the usual sliding sync loop and for getting the event for a
// notification (i.e. Element X Android). The verification events will be
// processed twice, meaning incorrect verification states will be found and the
// process will fail, especially with user verification.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mutual_sas_verification_with_notification_client_ignores_verification_events()
-> Result<()> {
    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };
    let alice = TestClientBuilder::new("alice")
        .use_sqlite()
        .encryption_settings(encryption_settings)
        .build()
        .await?;

    let alice_sync_service =
        Arc::new(SyncService::builder(alice.clone()).build().await.expect("Wat"));

    let bob = TestClientBuilder::new("bob")
        .use_sqlite()
        .encryption_settings(encryption_settings)
        .build()
        .await?;

    let bob_sync_service = Arc::new(SyncService::builder(bob.clone()).build().await.expect("Wat"));
    let bob_id = bob.user_id().expect("Bob should be logged in by now");

    alice.encryption().wait_for_e2ee_initialization_tasks().await;
    bob.encryption().wait_for_e2ee_initialization_tasks().await;

    alice_sync_service.start().await;
    bob_sync_service.start().await;

    warn!("alice's device: {}", alice.device_id().unwrap());
    warn!("bob's device: {}", bob.device_id().unwrap());

    // Set up the test: Alice creates the DM room, and invites Bob, who joins
    let invite = vec![bob_id.to_owned()];
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    let alice_room = alice.create_room(request).await?;
    alice_room.enable_encryption().await?;
    let room_id = alice_room.room_id();

    warn!("alice has created and enabled encryption in the room");

    timeout(
        async {
            loop {
                if let Some(room) = bob.get_room(room_id) {
                    room.join().await.expect("We should be able to join a room");
                    return;
                }
            }
        },
        Duration::from_secs(1),
    )
    .await
    .expect("Bob should have joined the room");

    bob_sync_service.stop().await;

    warn!("alice and bob are both aware of each other in the e2ee room");

    alice_room
        .send(RoomMessageEventContent::text_plain("Hello Bob"))
        .await
        .expect("We should be able to send a message to the room");

    let alice_bob_identity = alice
        .encryption()
        .get_user_identity(bob_id)
        .await
        .expect("We should be able to fetch an identity from the store")
        .expect("Bob's identity should be known by now");

    warn!("alice has found bob's identity");

    let alice_verification_request = alice_bob_identity.request_verification().await?;

    // The notification client must use the `SingleProcess` setup
    let notification_client = NotificationClient::new(
        bob.clone(),
        NotificationProcessSetup::SingleProcess { sync_service: bob_sync_service.clone() },
    )
    .await
    .expect("couldn't create notification client");

    let event_id = OwnedEventId::try_from(alice_verification_request.flow_id())
        .expect("We should be able to get the event id from the verification flow id");

    // Simulate getting the event for a notification
    let _ = notification_client.get_notification_with_sliding_sync(room_id, &event_id).await;

    let verification = bob
        .encryption()
        .get_verification_request(
            alice_verification_request.own_user_id(),
            alice_verification_request.flow_id(),
        )
        .await;

    // If the ignore_verification_events parameter is true in NotificationClient,
    // no verification request should have been received
    assert!(verification.is_none());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mutual_sas_verification() -> Result<()> {
    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };
    let alice = SyncTokenAwareClient::new(
        TestClientBuilder::new("alice")
            .use_sqlite()
            .encryption_settings(encryption_settings)
            .build()
            .await?,
    );
    let bob = SyncTokenAwareClient::new(
        TestClientBuilder::new("bob")
            .use_sqlite()
            .encryption_settings(encryption_settings)
            .build()
            .await?,
    );

    warn!("alice's device: {}", alice.device_id().unwrap());
    warn!("bob's device: {}", bob.device_id().unwrap());

    let invite = vec![bob.user_id().unwrap().to_owned()];
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    let alice_room = alice.create_room(request).await?;
    alice_room.enable_encryption().await?;
    let room_id = alice_room.room_id();

    warn!("alice has created and enabled encryption in the room");

    bob.sync_once().await?;
    bob.get_room(room_id).unwrap().join().await?;

    alice.sync_once().await?;
    bob.sync_once().await?;

    warn!("alice and bob are both aware of each other in the e2ee room");

    // Bob adds the verification listeners.
    let bob_verification_request = Arc::new(Mutex::new(None));
    {
        let bvr = bob_verification_request.clone();
        bob.add_event_handler(
            |ev: ToDeviceKeyVerificationRequestEvent, client: Client| async move {
                let request = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                    .expect("Request object wasn't created");
                *bvr.lock().unwrap() = Some(request);
            },
        );

        let bvr = bob_verification_request.clone();
        bob.add_event_handler(|ev: OriginalSyncRoomMessageEvent, client: Client| async move {
            if let MessageType::VerificationRequest(_) = &ev.content.msgtype {
                let request = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.event_id)
                    .await
                    .expect("Request object wasn't created");
                *bvr.lock().unwrap() = Some(request);
            }
        });
    }

    warn!("bob has set up verification listeners");

    let alice_bob_identity = alice
        .encryption()
        .get_user_identity(bob.user_id().unwrap())
        .await?
        .expect("alice knows bob's identity");

    warn!("alice has found bob's identity");

    let alice_verification_request = alice_bob_identity.request_verification().await?;

    assert!(!alice_verification_request.is_passive());
    assert!(alice_verification_request.we_started());
    assert_eq!(alice_verification_request.room_id(), Some(room_id));

    warn!("alice has started verification");

    bob.sync_once().await?;
    let bob_verification_request = bob_verification_request
        .lock()
        .unwrap()
        .take()
        .expect("bob received a verification request");

    warn!("bob has received the verification request");

    assert_eq!(bob_verification_request.room_id(), Some(room_id));

    assert!(!bob_verification_request.is_done());

    assert!(!bob_verification_request.is_cancelled());
    assert!(bob_verification_request.cancel_info().is_none());

    assert!(!bob_verification_request.is_ready());
    assert!(!bob_verification_request.is_passive());
    assert!(!bob_verification_request.we_started());

    assert_eq!(bob_verification_request.own_user_id(), bob.user_id().unwrap());
    assert_eq!(bob_verification_request.other_user_id(), alice.user_id().unwrap());
    assert!(!bob_verification_request.is_self_verification());

    assert_matches!(bob_verification_request.state(), VerificationRequestState::Requested { .. });
    let _flow_id = bob_verification_request.flow_id();

    // Bob notifies Alice he accepts the verification process.
    bob_verification_request.accept().await.unwrap();
    let our_methods = assert_matches!(bob_verification_request.state(), VerificationRequestState::Ready { our_methods, .. } => our_methods);
    assert!(bob_verification_request.is_ready());

    assert_eq!(bob_verification_request.their_supported_methods(), Some(our_methods));

    warn!("bob has accepted the verification request");

    // Alice receives the accept, and moves to the ready state.
    assert_matches!(alice_verification_request.state(), VerificationRequestState::Created { .. });
    alice.sync_once().await.unwrap();
    assert_matches!(alice_verification_request.state(), VerificationRequestState::Ready { .. });

    let alice_sas =
        alice_verification_request.start_sas().await?.expect("must have a sas verification");

    assert_eq!(alice_sas.own_user_id(), alice.user_id().unwrap());
    assert_eq!(alice_sas.other_user_id(), bob.user_id().unwrap());
    assert_eq!(alice_sas.room_id(), Some(room_id));

    assert!(alice_sas.started_from_request());
    assert!(!alice_sas.is_self_verification());
    assert!(alice_sas.we_started());
    assert!(!alice_sas.is_done());
    assert!(!alice_sas.is_cancelled());
    assert!(alice_sas.cancel_info().is_none());
    assert!(!alice_sas.supports_emoji());
    assert!(alice_sas.emoji().is_none());
    assert!(alice_sas.decimals().is_none());

    bob.sync_once().await?;

    let bob_verification = assert_matches!(bob_verification_request.state(), VerificationRequestState::Transitioned { verification } => verification);

    assert!(!bob_verification.is_done());
    assert!(!bob_verification.is_cancelled());
    assert!(!bob_verification.is_self_verification());
    assert!(!bob_verification.we_started());
    assert!(bob_verification.cancel_info().is_none());
    assert_eq!(bob_verification.own_user_id(), bob.user_id().unwrap());
    assert_eq!(bob_verification.other_user_id(), alice.user_id().unwrap());
    assert_eq!(bob_verification.room_id(), Some(room_id));

    let bob_sas = bob_verification.sas().unwrap();

    assert_matches!(alice_sas.state(), SasState::Created { .. });
    assert_matches!(bob_sas.state(), SasState::Started { .. });

    bob_sas.accept().await?;
    assert_matches!(bob_sas.state(), SasState::Accepted { .. });
    alice.sync_once().await?;
    assert_matches!(alice_sas.state(), SasState::Accepted { .. });
    assert!(alice_sas.supports_emoji());

    assert!(!alice_sas.can_be_presented());
    assert!(!bob_sas.can_be_presented());

    // Let a little crypto messages dance happen.
    alice.sync_once().await?;
    bob.sync_once().await?;
    assert_matches!(alice_sas.state(), SasState::Accepted { .. });
    assert_matches!(bob_sas.state(), SasState::KeysExchanged { .. });

    alice.sync_once().await?;
    let alice_emojis =
        assert_matches!(alice_sas.state(), SasState::KeysExchanged { emojis, .. } => emojis)
            .expect("alice received emojis");
    let bob_emojis =
        assert_matches!(bob_sas.state(), SasState::KeysExchanged { emojis, .. } => emojis)
            .expect("bob received emojis");

    assert!(alice_sas.can_be_presented());
    assert!(bob_sas.can_be_presented());

    assert_eq!(format_emojis(alice_emojis.emojis), format_emojis(bob_emojis.emojis));

    alice_sas.confirm().await?;
    bob_sas.confirm().await?;
    assert_matches!(alice_sas.state(), SasState::Confirmed);
    assert_matches!(bob_sas.state(), SasState::Confirmed);

    // Moar crypto dancing.
    alice.sync_once().await?;
    bob.sync_once().await?;
    assert_matches!(alice_sas.state(), SasState::Confirmed);
    assert_matches!(bob_sas.state(), SasState::Done { .. });

    alice.sync_once().await?;
    assert_matches!(alice_sas.state(), SasState::Done { .. });
    assert_matches!(bob_sas.state(), SasState::Done { .. });

    // Wait for remote echos for verification status requests.
    alice.sync_once().await?;
    bob.sync_once().await?;

    assert!(!bob_verification_request.is_cancelled());
    assert!(!alice_verification_request.is_cancelled());

    assert!(bob_verification_request.is_done());
    assert!(alice_verification_request.is_done());
    assert!(bob_sas.is_done());
    assert!(alice_sas.is_done());

    // Both users appear as verified to each other.
    let alice_bob_ident =
        alice.encryption().get_user_identity(bob.user_id().unwrap()).await?.unwrap();
    assert!(alice_bob_ident.is_verified());

    let bob_alice_ident =
        bob.encryption().get_user_identity(alice.user_id().unwrap()).await?.unwrap();
    assert!(bob_alice_ident.is_verified());

    // Both user devices appear as verified to the other user.
    let alice_bob_device = alice_sas.other_device();
    assert_eq!(alice_bob_device.user_id(), bob.user_id().unwrap());
    assert_eq!(alice_bob_device.device_id(), bob.device_id().unwrap());
    assert_eq!(alice_bob_device.local_trust_state(), LocalTrust::Unset);

    let alice_bob_device = alice
        .encryption()
        .get_device(bob.user_id().unwrap(), bob.device_id().unwrap())
        .await?
        .unwrap();
    assert!(alice_bob_device.is_verified());
    assert_eq!(alice_bob_device.local_trust_state(), LocalTrust::Verified);
    assert!(alice_bob_device.is_locally_trusted());
    assert!(!alice_bob_device.is_blacklisted());

    let bob_alice_device = bob_sas.other_device();
    assert_eq!(bob_alice_device.user_id(), alice.user_id().unwrap());
    assert_eq!(bob_alice_device.device_id(), alice.device_id().unwrap());
    assert_eq!(bob_alice_device.local_trust_state(), LocalTrust::Unset);

    let bob_alice_device = bob
        .encryption()
        .get_device(alice.user_id().unwrap(), alice.device_id().unwrap())
        .await?
        .unwrap();
    assert!(bob_alice_device.is_verified());
    assert_eq!(bob_alice_device.local_trust_state(), LocalTrust::Verified);
    assert!(bob_alice_device.is_locally_trusted());
    assert!(!bob_alice_device.is_blacklisted());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mutual_qrcode_verification() -> Result<()> {
    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };
    let alice = SyncTokenAwareClient::new(
        TestClientBuilder::new("alice")
            .use_sqlite()
            .encryption_settings(encryption_settings)
            .build()
            .await?,
    );
    let bob = SyncTokenAwareClient::new(
        TestClientBuilder::new("bob")
            .use_sqlite()
            .encryption_settings(encryption_settings)
            .build()
            .await?,
    );

    warn!("alice's device: {}", alice.device_id().unwrap());
    warn!("bob's device: {}", bob.device_id().unwrap());

    let invite = vec![bob.user_id().unwrap().to_owned()];
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    let alice_room = alice.create_room(request).await?;
    alice_room.enable_encryption().await?;
    let room_id = alice_room.room_id();

    warn!("alice has created and enabled encryption in the room");

    bob.sync_once().await?;
    bob.get_room(room_id).unwrap().join().await?;

    alice.sync_once().await?;
    bob.sync_once().await?;

    warn!("alice and bob are both aware of each other in the e2ee room");

    // Bob adds the verification listeners.
    let bob_verification_request = Arc::new(Mutex::new(None));
    {
        let bvr = bob_verification_request.clone();
        bob.add_event_handler(
            |ev: ToDeviceKeyVerificationRequestEvent, client: Client| async move {
                let request = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.content.transaction_id)
                    .await
                    .expect("Request object wasn't created");
                *bvr.lock().unwrap() = Some(request);
            },
        );

        let bvr = bob_verification_request.clone();
        bob.add_event_handler(|ev: OriginalSyncRoomMessageEvent, client: Client| async move {
            if let MessageType::VerificationRequest(_) = &ev.content.msgtype {
                let request = client
                    .encryption()
                    .get_verification_request(&ev.sender, &ev.event_id)
                    .await
                    .expect("Request object wasn't created");
                *bvr.lock().unwrap() = Some(request);
            }
        });
    }

    warn!("bob has set up verification listeners");

    let alice_bob_identity = alice
        .encryption()
        .get_user_identity(bob.user_id().unwrap())
        .await?
        .expect("alice knows bob's identity");

    warn!("alice has found bob's identity");

    let alice_verification_request = alice_bob_identity
        .request_verification_with_methods(vec![
            VerificationMethod::SasV1,
            VerificationMethod::QrCodeScanV1,
            VerificationMethod::QrCodeShowV1,
            VerificationMethod::ReciprocateV1,
        ])
        .await?;

    assert!(!alice_verification_request.is_passive());
    assert!(alice_verification_request.we_started());
    assert_eq!(alice_verification_request.room_id(), Some(room_id));

    warn!("alice has started verification");

    bob.sync_once().await?;
    let bob_verification_request = bob_verification_request
        .lock()
        .unwrap()
        .take()
        .expect("bob received a verification request");

    warn!("bob has received the verification request");

    assert_eq!(bob_verification_request.room_id(), Some(room_id));

    assert!(!bob_verification_request.is_done());

    assert!(!bob_verification_request.is_cancelled());
    assert!(bob_verification_request.cancel_info().is_none());

    assert!(!bob_verification_request.is_ready());
    assert!(!bob_verification_request.is_passive());
    assert!(!bob_verification_request.we_started());

    assert_eq!(bob_verification_request.own_user_id(), bob.user_id().unwrap());
    assert_eq!(bob_verification_request.other_user_id(), alice.user_id().unwrap());
    assert!(!bob_verification_request.is_self_verification());

    assert_matches!(bob_verification_request.state(), VerificationRequestState::Requested { .. });
    let _flow_id = bob_verification_request.flow_id();

    // Bob notifies Alice he accepts the verification process.
    bob_verification_request.accept().await.unwrap();
    assert_matches!(bob_verification_request.state(), VerificationRequestState::Ready { .. });
    assert!(bob_verification_request.is_ready());

    warn!("bob has accepted the verification request");

    // Alice receives the accept, and moves to the ready state.
    assert_matches!(alice_verification_request.state(), VerificationRequestState::Created { .. });
    alice.sync_once().await.unwrap();
    assert_matches!(alice_verification_request.state(), VerificationRequestState::Ready { .. });

    let bob_qr =
        bob_verification_request.generate_qr_code().await?.expect("must have a qr verification");

    assert_matches!(bob_qr.state(), QrVerificationState::Started);

    warn!("bob has generated the QR code");

    let bob_verification = assert_matches!(bob_verification_request.state(), VerificationRequestState::Transitioned { verification } => verification);

    assert!(!bob_verification.is_done());
    assert!(!bob_verification.is_cancelled());
    assert!(!bob_verification.is_self_verification());
    assert!(!bob_verification.we_started());
    assert!(bob_verification.cancel_info().is_none());
    assert_eq!(bob_verification.own_user_id(), bob.user_id().unwrap());
    assert_eq!(bob_verification.other_user_id(), alice.user_id().unwrap());
    assert_eq!(bob_verification.room_id(), Some(room_id));

    let qr_code_bytes = bob_qr.to_bytes()?;
    let alice_qr = alice_verification_request
        .scan_qr_code(QrVerificationData::from_bytes(qr_code_bytes)?)
        .await?
        .expect("must have a qr verification");

    warn!("alice has scanned the QR code");

    assert_eq!(alice_qr.own_user_id(), alice.user_id().unwrap());
    assert_eq!(alice_qr.other_user_id(), bob.user_id().unwrap());
    assert_eq!(alice_qr.room_id(), Some(room_id));

    assert!(!alice_qr.is_self_verification());
    assert!(alice_qr.we_started());
    assert!(!alice_qr.is_done());
    assert!(!alice_qr.is_cancelled());
    assert!(alice_qr.cancel_info().is_none());

    bob.sync_once().await?;

    assert_matches!(alice_qr.state(), QrVerificationState::Reciprocated);
    assert_matches!(bob_qr.state(), QrVerificationState::Scanned);

    bob_qr.confirm().await?;

    warn!("bob has confirmed the QR code scanning");

    alice.sync_once().await?;

    assert_matches!(bob_qr.state(), QrVerificationState::Confirmed);
    assert_matches!(alice_qr.state(), QrVerificationState::Done { .. });

    // Crypto dancing.
    alice.sync_once().await?;
    bob.sync_once().await?;
    assert_matches!(bob_qr.state(), QrVerificationState::Done { .. });
    assert_matches!(alice_qr.state(), QrVerificationState::Done { .. });

    // Wait for remote echos for verification status requests.
    alice.sync_once().await?;
    bob.sync_once().await?;

    assert!(!bob_verification_request.is_cancelled());
    assert!(!alice_verification_request.is_cancelled());

    assert!(bob_verification_request.is_done());
    assert!(alice_verification_request.is_done());
    assert!(bob_qr.is_done());
    assert!(alice_qr.is_done());

    // Both users appear as verified to each other.
    let alice_bob_ident =
        alice.encryption().get_user_identity(bob.user_id().unwrap()).await?.unwrap();
    assert!(alice_bob_ident.is_verified());

    let bob_alice_ident =
        bob.encryption().get_user_identity(alice.user_id().unwrap()).await?.unwrap();
    assert!(bob_alice_ident.is_verified());

    // Both user devices appear as verified to the other user.
    let alice_bob_device = alice_qr.other_device();
    assert_eq!(alice_bob_device.user_id(), bob.user_id().unwrap());
    assert_eq!(alice_bob_device.device_id(), bob.device_id().unwrap());
    assert_eq!(alice_bob_device.local_trust_state(), LocalTrust::Unset);

    let alice_bob_device = alice
        .encryption()
        .get_device(bob.user_id().unwrap(), bob.device_id().unwrap())
        .await?
        .unwrap();
    assert!(alice_bob_device.is_verified());
    assert!(!alice_bob_device.is_blacklisted());

    let bob_alice_device = bob_qr.other_device();
    assert_eq!(bob_alice_device.user_id(), alice.user_id().unwrap());
    assert_eq!(bob_alice_device.device_id(), alice.device_id().unwrap());

    let bob_alice_device = bob
        .encryption()
        .get_device(alice.user_id().unwrap(), alice.device_id().unwrap())
        .await?
        .unwrap();
    assert!(bob_alice_device.is_verified());
    assert!(!bob_alice_device.is_blacklisted());

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_encryption_missing_member_keys() -> Result<()> {
    let alice =
        SyncTokenAwareClient::new(TestClientBuilder::new("alice").use_sqlite().build().await?);
    let bob = SyncTokenAwareClient::new(TestClientBuilder::new("bob").use_sqlite().build().await?);

    let invite = vec![bob.user_id().unwrap().to_owned()];
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    let alice_room = alice.create_room(request).await?;
    alice_room.enable_encryption().await?;
    alice.sync_once().await?;

    warn!("alice has created and enabled encryption in the room");

    bob.sync_once().await?;
    bob.get_room(alice_room.room_id()).unwrap().join().await?;

    warn!("bob has joined");

    // New person joins the room.
    let carl =
        SyncTokenAwareClient::new(TestClientBuilder::new("carl").use_sqlite().build().await?);
    alice_room.invite_user_by_id(carl.user_id().unwrap()).await?;

    carl.sync_once().await?;
    carl.get_room(alice_room.room_id()).unwrap().join().await?;
    carl.sync_once().await?;

    warn!("carl has joined");

    // Bob sends message WITHOUT syncing.
    warn!("bob sends message...");
    let bob_room = bob.get_room(alice_room.room_id()).unwrap();
    let message = "Hello world!";
    let bob_message_content = Arc::new(Mutex::new(message));
    bob_room.send(RoomMessageEventContent::text_plain(message)).await?;
    warn!("bob is done sending the message");

    // Alice was in the room when Bob sent the message, so they'll see it.
    let alice_found_event = Arc::new(Mutex::new(false));
    {
        warn!("alice is looking for the decrypted message");

        let found_event_handler = alice_found_event.clone();
        let bob_message_content = bob_message_content.clone();
        alice.add_event_handler(move |event: SyncRoomMessageEvent| async move {
            warn!("Found a message \\o/ {event:?}");
            let MessageType::Text(text_content) = &event.as_original().unwrap().content.msgtype
            else {
                return;
            };
            if text_content.body == *bob_message_content.lock().unwrap() {
                *found_event_handler.lock().unwrap() = true;
            }
        });

        alice.sync_once().await?;

        let found = *alice_found_event.lock().unwrap();
        assert!(found, "event has not been found for alice");
    }

    // Bob wasn't aware of Carl's presence (no sync() since Carl joined), so Carl
    // won't see it first.
    let carl_found_event = Arc::new(Mutex::new(false));
    {
        warn!("carl is looking for the decrypted message");

        let found_event_handler = carl_found_event.clone();
        let bob_message_content = bob_message_content.clone();
        carl.add_event_handler(move |event: SyncRoomMessageEvent| async move {
            warn!("Found a message \\o/ {event:?}");
            let MessageType::Text(text_content) = &event.as_original().unwrap().content.msgtype
            else {
                return;
            };
            if text_content.body == *bob_message_content.lock().unwrap() {
                *found_event_handler.lock().unwrap() = true;
            }
        });

        carl.sync_once().await?;

        let found = *carl_found_event.lock().unwrap();
        assert!(!found, "event has been unexpectedly found for carl");
    }

    // Now Bob syncs, thus notices the presence of Carl.
    bob.sync_once().await?;

    warn!("bob sends another message...");
    let bob_room = bob.get_room(alice_room.room_id()).unwrap();
    let message = "Wassup";
    *bob_message_content.lock().unwrap() = message;
    bob_room.send(RoomMessageEventContent::text_plain(message)).await?;
    warn!("bob is done sending another message");

    {
        *alice_found_event.lock().unwrap() = false;
        alice.sync_once().await?;
        let found = *alice_found_event.lock().unwrap();
        assert!(found, "second message has not been found for alice");
    }

    {
        *carl_found_event.lock().unwrap() = false;
        carl.sync_once().await?;
        let found = *carl_found_event.lock().unwrap();
        assert!(found, "second message has not been found for carl");
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_failed_members_response() -> Result<()> {
    let alice =
        SyncTokenAwareClient::new(TestClientBuilder::new("alice").use_sqlite().build().await?);
    let bob = SyncTokenAwareClient::new(TestClientBuilder::new("bob").use_sqlite().build().await?);

    let invite = vec![bob.user_id().unwrap().to_owned()];
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    let alice_room = alice.create_room(request).await?;
    alice_room.enable_encryption().await?;
    alice.sync_once().await?;

    warn!("alice has created and enabled encryption in the room");

    bob.sync_once().await?;

    // Although we haven't joined the room yet, logic in `sync_members` looks at the
    // room's visibility first; since it may be unknown for this room, from the
    // point of view of Bob, it'll be assumed to be the default, aka shared. As
    // a result, `sync_members()` doesn't even spawn a network request, and
    // silently ignores the request.

    let result = bob.get_room(alice_room.room_id()).unwrap().sync_members().await;
    assert!(result.is_ok());

    bob.get_room(alice_room.room_id()).unwrap().join().await?;

    warn!("bob has joined");

    // Bob sends message WITHOUT syncing.
    warn!("bob sends message...");
    let bob_room = bob.get_room(alice_room.room_id()).unwrap();
    let message = "Hello world!";
    let bob_message_content = Arc::new(Mutex::new(message));
    bob_room.send(RoomMessageEventContent::text_plain(message)).await?;
    warn!("bob is done sending the message");

    // Alice sees the message.
    let alice_found_event = Arc::new(Mutex::new(false));
    warn!("alice is looking for the decrypted message");

    let found_event_handler = alice_found_event.clone();
    let bob_message_content = bob_message_content.clone();
    alice.add_event_handler(move |event: SyncRoomMessageEvent| async move {
        warn!("Found a message \\o/ {event:?}");
        let MessageType::Text(text_content) = &event.as_original().unwrap().content.msgtype else {
            return;
        };
        if text_content.body == *bob_message_content.lock().unwrap() {
            *found_event_handler.lock().unwrap() = true;
        }
    });

    alice.sync_once().await?;

    let found = *alice_found_event.lock().unwrap();
    assert!(found, "event has not been found for alice");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_backup_enable_new_user() -> Result<()> {
    let encryption_settings =
        EncryptionSettings { auto_enable_backups: true, ..Default::default() };

    let alice = SyncTokenAwareClient::new(
        TestClientBuilder::new("alice")
            .use_sqlite()
            .encryption_settings(encryption_settings)
            .build()
            .await?,
    );

    alice.encryption().wait_for_e2ee_initialization_tasks().await;

    assert!(
        alice.encryption().backups().are_enabled().await,
        "Backups should have been enabled automatically."
    );

    assert_eq!(
        alice.encryption().backups().state(),
        BackupState::Enabled,
        "The backup state should now be BackupState::Enabled"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_cross_signing_bootstrap() -> Result<()> {
    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

    let alice = SyncTokenAwareClient::new(
        TestClientBuilder::new("alice")
            .use_sqlite()
            .encryption_settings(encryption_settings)
            .build()
            .await?,
    );

    alice.encryption().wait_for_e2ee_initialization_tasks().await;

    let status = alice
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should know our cross-signing status by now.");

    assert!(status.is_complete(), "We should have all private cross-signing keys available.");

    // We need to sync to get the remote echo of the upload of the device keys.
    alice.sync_once().await?;

    let own_device = alice.encryption().get_own_device().await?.unwrap();

    assert!(
        own_device.is_cross_signed_by_owner(),
        "Since we bootstrapped cross-signing, our own device should have been \
         signed by the cross-signing keys."
    );

    Ok(())
}

pub(super) async fn assert_can_perform_interactive_verification(
    username: impl AsRef<str>,
    backup_download_strategy: BackupDownloadStrategy,
    enable_history_share_on_invite: bool,
) -> Result<(SyncTokenAwareClient, SyncTokenAwareClient, OwnedRoomId, OwnedEventId)> {
    let encryption_settings = EncryptionSettings {
        auto_enable_cross_signing: true,
        auto_enable_backups: true,
        backup_download_strategy,
    };

    let first_client = SyncTokenAwareClient::new(
        TestClientBuilder::new(username)
            .encryption_settings(encryption_settings)
            .enable_share_history_on_invite(enable_history_share_on_invite)
            .build()
            .await?,
    );

    let user_id = first_client.user_id().expect("We should have access to the user id now");

    let room_first_client = first_client
        .create_room(assign!(CreateRoomRequest::new(), {
            preset: Some(RoomPreset::PublicChat),
        }))
        .await?;
    room_first_client.enable_encryption().await?;
    first_client.sync_once().await?;

    first_client.encryption().recovery().enable().await?;

    assert_eq!(first_client.encryption().recovery().state(), RecoveryState::Enabled);

    let response = room_first_client
        .send(RoomMessageEventContent::text_plain("It's a secret to everybody"))
        .await?
        .response;

    let event_id = response.event_id;

    warn!("The first device has created and enabled encryption in the room and sent an event");

    let second_client = SyncTokenAwareClient::new(
        TestClientBuilder::with_exact_username(user_id.localpart().to_owned())
            .encryption_settings(encryption_settings)
            .enable_share_history_on_invite(enable_history_share_on_invite)
            .build()
            .await?,
    );

    second_client.encryption().wait_for_e2ee_initialization_tasks().await;

    // The second client doesn't have access to the backup, nor is recovery in the
    // enabled state.
    assert_eq!(second_client.encryption().recovery().state(), RecoveryState::Incomplete);
    assert_eq!(second_client.encryption().backups().state(), BackupState::Unknown);

    warn!("The first device: {}", first_client.device_id().unwrap());
    warn!("The second device: {}", second_client.device_id().unwrap());
    assert_ne!(first_client.device_id().unwrap(), second_client.device_id().unwrap());

    second_client.sync_once().await?;

    let seconds_first_device = second_client
        .encryption()
        .get_device(second_client.user_id().unwrap(), first_client.device_id().unwrap())
        .await?
        .expect("We should have access to the first device once we have synced");

    // The first client is not verified from the point of view of the second client.
    assert!(!seconds_first_device.is_verified());

    // Make the first client aware of the device we're requesting verification for
    first_client.sync_once().await?;

    // Let's send out a request to verify with each other.
    let seconds_verification_request = seconds_first_device.request_verification().await?;
    let flow_id = seconds_verification_request.flow_id();

    first_client.sync_once().await?;
    let firsts_verification_request = first_client
        .encryption()
        .get_verification_request(user_id, flow_id)
        .await
        .expect("The verification should have been requested");

    assert_matches!(seconds_verification_request.state(), VerificationRequestState::Created { .. });
    assert_matches!(
        firsts_verification_request.state(),
        VerificationRequestState::Requested { .. }
    );
    warn!("The first device is accepting the verification request");
    firsts_verification_request.accept().await?;

    first_client.sync_once().await?;

    assert_matches!(firsts_verification_request.state(), VerificationRequestState::Ready { .. });
    let firsts_sas = firsts_verification_request
        .start_sas()
        .await?
        .expect("We should be able to start the SAS verification");

    second_client.sync_once().await?;
    assert_let!(
        VerificationRequestState::Transitioned { verification: Verification::SasV1(seconds_sas) } =
            seconds_verification_request.state()
    );

    seconds_sas.accept().await?;

    // We need to sync a couple of times so the clients exchange the shared secret.
    first_client.sync_once().await?;
    second_client.sync_once().await?;
    first_client.sync_once().await?;

    assert_eq!(
        firsts_sas.emoji().expect("The firsts sas should be presentable"),
        seconds_sas.emoji().expect("The seconds sas should be presentable"),
        "The emojis should match"
    );

    // Confirm that the emojis match.
    firsts_sas.confirm().await?;
    seconds_sas.confirm().await?;

    // After both sides confirm, we need a couple more syncs to exchange the final
    // verification events.
    second_client.sync_once().await?;
    first_client.sync_once().await?;
    second_client.sync_once().await?;

    // And we're done, the verification dance is completed.
    assert!(seconds_sas.is_done());
    assert!(firsts_sas.is_done());

    let second_device = second_client.encryption().get_own_device().await?.unwrap();

    // The first device has signed the second one.
    assert!(second_device.is_cross_signed_by_owner());
    assert!(
        !second_client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "We should not have received our cross-signing keys yet."
    );
    assert_eq!(
        second_client.encryption().backups().state(),
        BackupState::Unknown,
        "The backup should not have been enabled yet."
    );
    assert_eq!(
        second_client.encryption().recovery().state(),
        RecoveryState::Incomplete,
        "The recovery state should be in the Incomplete state, since we have not yet received all secrets"
    );

    // We still need to gossip the secrets from one device to the other, the first
    // device syncs to receive the gossip requests, then the second device syncs
    // to receive the secrets.
    first_client.sync_once().await?;
    warn!("The second client is doing its final sync");
    second_client.sync_once().await?;

    assert!(
        second_client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "We should have received all the cross-signing keys from the first device"
    );
    assert_eq!(
        second_client.encryption().backups().state(),
        BackupState::Enabled,
        "We should have enabled the backup after we received the backup key from the first device"
    );
    assert_eq!(
        second_client.encryption().recovery().state(),
        RecoveryState::Enabled,
        "The recovery state should be in the Enabled state, since we have all the secrets"
    );

    Ok((first_client, second_client, room_first_client.room_id().to_owned(), event_id))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_secret_gossip_after_interactive_verification() -> Result<()> {
    let (_, second_client, room_id, event_id) = assert_can_perform_interactive_verification(
        "alice_gossip_test",
        BackupDownloadStrategy::OneShot,
        false,
    )
    .await?;

    // Let's now check if we can decrypt the event that was sent before our
    // device was created.
    let room = second_client
        .get_room(&room_id)
        .expect("The second client should know about the room as well");

    let timeline_event = room.event(&event_id, None).await?;
    timeline_event
        .encryption_info()
        .expect("The event should have been encrypted and successfully decrypted.");

    let event: OriginalSyncMessageLikeEvent<RoomMessageEventContent> =
        timeline_event.raw().deserialize_as_unchecked()?;
    let message = event.content.msgtype;

    assert_let!(MessageType::Text(message) = message);

    assert_eq!(
        message.body, "It's a secret to everybody",
        "The decrypted message should match the text we encrypted."
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_recovery_disabling_deletes_secret_storage_secrets() -> Result<()> {
    let encryption_settings = EncryptionSettings {
        auto_enable_cross_signing: true,
        auto_enable_backups: true,
        ..Default::default()
    };
    let client = SyncTokenAwareClient::new(
        TestClientBuilder::new("alice_recovery_deletion_test")
            .encryption_settings(encryption_settings)
            .build()
            .await?,
    );

    debug!("Enabling recovery");

    client.encryption().wait_for_e2ee_initialization_tasks().await;
    client.encryption().recovery().enable().await?;

    // Let's wait for recovery to become enabled.
    let mut recovery_state = client.encryption().recovery().state_stream();

    debug!("Checking that recovery has been enabled");

    assert_next_eq_with_timeout!(
        recovery_state,
        RecoveryState::Enabled,
        "Recovery should have been enabled"
    );

    debug!("Checking that the secrets have been stored on the server");

    for event_type in Recovery::KNOWN_SECRETS {
        let event_type = GlobalAccountDataEventType::from(event_type.clone());
        let event = client
            .account()
            .fetch_account_data(event_type.clone())
            .await?
            .expect("The secret event should still exist");

        let event = event
            .deserialize_as_unchecked::<SecretEventContent>()
            .expect("We should be able to deserialize the content of known secrets");

        assert!(
            !event.encrypted.is_empty(),
            "The known secret {event_type} should exist on the server"
        );
    }

    debug!("Disabling recovery");

    client.encryption().recovery().disable().await?;

    debug!("Checking that the secrets have been removed from the server");

    for event_type in Recovery::KNOWN_SECRETS {
        let event_type = GlobalAccountDataEventType::from(event_type.clone());
        let event = client
            .account()
            .fetch_account_data(event_type.clone())
            .await?
            .expect("The secret event should still exist");

        let event = event
            .deserialize_as_unchecked::<SecretEventContent>()
            .expect("We should be able to deserialize the content since that's what we uploaded");

        assert!(
            event.encrypted.is_empty(),
            "The known secret {event_type} should have been deleted from the server"
        );
    }

    Ok(())
}
