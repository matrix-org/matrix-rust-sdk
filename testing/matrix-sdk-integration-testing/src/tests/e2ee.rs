use std::sync::{Arc, Mutex};

use anyhow::Result;
use assert_matches::assert_matches;
use assign::assign;
use matrix_sdk::{
    crypto::{format_emojis, SasState},
    encryption::{verification::VerificationRequestState, LocalTrust},
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::{
            key::verification::request::ToDeviceKeyVerificationRequestEvent,
            room::message::{
                MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent,
                SyncRoomMessageEvent,
            },
        },
    },
    Client,
};
use tracing::warn;

use crate::helpers::{SyncTokenAwareClient, TestClientBuilder};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mutual_verification() -> Result<()> {
    let alice = SyncTokenAwareClient::new(
        TestClientBuilder::new("alice")
            .randomize_username()
            .use_sqlite()
            .bootstrap_cross_signing()
            .build()
            .await?,
    );
    let bob = SyncTokenAwareClient::new(
        TestClientBuilder::new("bob")
            .randomize_username()
            .use_sqlite()
            .bootstrap_cross_signing()
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

    warn!("alice has created and enabled encryption in the room");

    bob.sync_once().await?;
    bob.get_room(alice_room.room_id()).unwrap().join().await?;

    alice.sync_once().await?;

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

    warn!("alice has started verification");

    bob.sync_once().await?;
    let bob_verification_request = bob_verification_request
        .lock()
        .unwrap()
        .take()
        .expect("bob received a verification request");

    warn!("bob has received the verification request");

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

    let bob_sas = bob_verification.sas().unwrap();

    assert_matches!(alice_sas.state(), SasState::Started { .. });
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
async fn test_encryption_missing_member_keys() -> Result<()> {
    let alice = SyncTokenAwareClient::new(
        TestClientBuilder::new("alice").randomize_username().use_sqlite().build().await?,
    );
    let bob = SyncTokenAwareClient::new(
        TestClientBuilder::new("bob").randomize_username().use_sqlite().build().await?,
    );

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
    let carl = SyncTokenAwareClient::new(
        TestClientBuilder::new("carl").randomize_username().use_sqlite().build().await?,
    );
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
    let alice = SyncTokenAwareClient::new(
        TestClientBuilder::new("alice").randomize_username().use_sqlite().build().await?,
    );
    let bob = SyncTokenAwareClient::new(
        TestClientBuilder::new("bob").randomize_username().use_sqlite().build().await?,
    );

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

    // Cause a failure of a sync_members request by asking for members before
    // joining. Since this is a private DM room, it will fail with a 401, as
    // we're not authorized to look at state history.
    let result = bob.get_room(alice_room.room_id()).unwrap().sync_members().await;
    assert!(result.is_err());

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
