use std::{ops::Deref, sync::Arc, time::Duration};

use anyhow::Result;
use assert_matches2::{assert_let, assert_matches};
use assign::assign;
use eyeball_im::VectorDiff;
use futures::{FutureExt, StreamExt, future, pin_mut};
use matrix_sdk::{
    Client, assert_decrypted_message_eq, assert_next_with_timeout,
    deserialized_responses::TimelineEventKind,
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    room::power_levels::RoomPowerLevelChanges,
    ruma::{
        EventId, OwnedEventId, OwnedRoomId,
        api::client::{
            room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
            uiaa::Password,
        },
        events::room::{
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            message::RoomMessageEventContent,
        },
    },
    timeout::timeout,
};
use matrix_sdk_base::crypto::types::events::UtdCause;
use matrix_sdk_common::deserialized_responses::{
    DeviceLinkProblem, ProcessedToDeviceEvent, UnableToDecryptReason::MissingMegolmSession,
    VerificationLevel, VerificationState, WithheldCode,
};
use matrix_sdk_ui::{
    Timeline,
    sync_service::SyncService,
    timeline::{
        EncryptedMessage, MsgLikeContent, MsgLikeKind, RoomExt, TimelineDetails, TimelineItem,
        TimelineItemContent,
    },
};
use similar_asserts::assert_eq;
use tracing::{Instrument, info};

use crate::{
    helpers::{SyncTokenAwareClient, TestClientBuilder, wait_for_room},
    tests::e2ee::assert_can_perform_interactive_verification,
};

/// When we invite another user to a room with "joined" history visibility, we
/// share the encryption history.
///
/// Pre-"exclude insecure devices" test variant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_history_share_on_invite() -> Result<()> {
    test_history_share_on_invite_helper(false).await
}

/// When we invite another user to a room with "joined" history visibility, we
/// share the encryption history, even when "exclude insecure devices" is
/// enabled.
///
/// Regression test for https://github.com/matrix-org/matrix-rust-sdk/issues/5613
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_history_share_on_invite_exclude_insecure_devices() -> Result<()> {
    test_history_share_on_invite_helper(true).await
}

/// Common implementation for [`test_history_share_on_invite`] and
/// [`test_history_share_on_invite_exclude_insecure_devices].
async fn test_history_share_on_invite_helper(exclude_insecure_devices: bool) -> Result<()> {
    let alice_span = tracing::info_span!("alice");
    let bob_span = tracing::info_span!("bob");

    let alice = create_encryption_enabled_client("alice", exclude_insecure_devices)
        .instrument(alice_span.clone())
        .await?;

    let alice_sync_service = start_client_sync_service(&alice_span, &alice).await;

    let bob = create_encryption_enabled_client("bob", exclude_insecure_devices)
        .instrument(bob_span.clone())
        .await?;

    // Alice creates a room ...
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            preset: Some(RoomPreset::PublicChat),
        }))
        .await?;
    alice_room.enable_encryption().await?;

    info!(room_id = ?alice_room.room_id(), "Alice has created and enabled encryption in the room");

    // ... and sends a message
    let event_id = alice_room
        .send(RoomMessageEventContent::text_plain("Hello Bob"))
        .await
        .expect("We should be able to send a message to the room")
        .response
        .event_id;

    let bundle_stream = bob
        .encryption()
        .historic_room_key_stream()
        .await
        .expect("We should be able to get the bundle stream");

    // Alice invites Bob to the room
    alice_room.invite_user_by_id(bob.user_id().unwrap()).await?;

    // Alice is done. Bob has been invited and the room key bundle should have been
    // sent out. Let's log her out, so we know that this feature works even when the
    // sender device has been deleted (and to reduce the amount of noise in the
    // logs).
    alice_sync_service.stop().await;
    alice.logout().instrument(alice_span.clone()).await?;

    // Workaround for https://github.com/matrix-org/matrix-rust-sdk/issues/5770: Bob needs a copy of
    // Alice's identity.
    bob.encryption()
        .request_user_identity(alice.user_id().unwrap())
        .instrument(bob_span.clone())
        .await?;

    let bob_response = bob.sync_once().instrument(bob_span.clone()).await?;

    // Bob should have received a to-device event with the payload
    assert_received_room_key_bundle(bob_response);

    bob.get_room(alice_room.room_id()).expect("Bob should have received the invite");

    pin_mut!(bundle_stream);

    let info = bundle_stream
        .next()
        .now_or_never()
        .flatten()
        .expect("We should be notified about the received bundle");

    assert_eq!(Some(info.sender.deref()), alice.user_id());
    assert_eq!(info.room_id, alice_room.room_id());

    let bob_room = bob
        .join_room_by_id(alice_room.room_id())
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to accept the invitation from Alice");

    let event = bob_room
        .event(&event_id, None)
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to fetch the historic event");

    assert_decrypted_message_eq!(
        event,
        "Hello Bob",
        "The decrypted event should match the message Alice has sent"
    );

    // We should be able to find the event using the high level timeline API, and
    // inspect who forwarded us the keys to decrypt.

    let alice_id = alice.user_id().unwrap();
    let alice_display_name =
        alice.account().get_display_name().await?.expect("Alice should have a display name");

    let bob_timeline = bob_room.timeline().await?;
    bob.sync_once().instrument(bob_span.clone()).await?;

    let item = assert_event_received(&bob_timeline, &event_id, "Hello Bob").await;
    let event = item.as_event().expect("The timeline item should be an event");

    assert_eq!(
        event.forwarder().expect("We should be able to access the forwarder's ID"),
        alice_id.as_str()
    );
    assert_let!(
        Some(TimelineDetails::Ready(profile)) = event.forwarder_profile(),
        "We should be able to access the forwarder's profile"
    );
    assert_eq!(
        profile
            .display_name
            .as_ref()
            .expect("We should be able to access the forwarder's display name"),
        &alice_display_name
    );

    Ok(())
}

/// When we invite another user to a room with "joined" history visibility, we
/// share the encryption history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_history_share_on_invite_pin_violation() -> Result<()> {
    let alice_span = tracing::info_span!("alice");
    let bob_span = tracing::info_span!("bob");

    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

    let alice = TestClientBuilder::new("alice")
        .use_sqlite()
        .encryption_settings(encryption_settings)
        .enable_share_history_on_invite(true)
        .build()
        .await?;

    let alice_user_id = alice.user_id().expect("We should have access to Alice's user id");

    let sync_service_span = tracing::info_span!(parent: &alice_span, "sync_service");
    let alice_sync_service = SyncService::builder(alice.clone())
        .with_parent_span(sync_service_span)
        .build()
        .await
        .expect("Could not build alice sync service");

    alice.encryption().wait_for_e2ee_initialization_tasks().await;
    alice_sync_service.start().await;

    // Bob first creates a room so we can get hold of Alice's identity and verify
    // it.

    let bob = TestClientBuilder::new("bob")
        .encryption_settings(encryption_settings)
        .enable_share_history_on_invite(true)
        .build()
        .await?;

    let bob_sync_service_span = tracing::info_span!(parent: &bob_span, "sync_service");
    let bob_sync_service = SyncService::builder(bob.clone())
        .with_parent_span(bob_sync_service_span)
        .build()
        .await
        .expect("Could not build alice sync service");

    bob_sync_service.start().await;

    let bob_room = bob
        .create_room(assign!(CreateRoomRequest::new(), {
            preset: Some(RoomPreset::PublicChat),
        }))
        .await?;
    bob_room.enable_encryption().await?;

    // We invite Alice and try to send a message. This will force a /keys/query to
    // fetch the identity.
    bob_room.invite_user_by_id(alice_user_id).await?;
    bob_room.send(RoomMessageEventContent::text_plain("Hello Alice")).await?;

    let alice_identity = bob
        .encryption()
        .get_user_identity(alice.user_id().unwrap())
        .await?
        .expect("Bob should have access to Alice's identity");

    info!("Bob is verifying Alice");

    alice_identity.verify().await?;

    alice
        .get_room(bob_room.room_id())
        .expect("Alice should have access to bob's room")
        .join()
        .instrument(alice_span.clone())
        .await?;

    // Alice creates a room ...
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            preset: Some(RoomPreset::PublicChat),
        }))
        .await?;
    alice_room.enable_encryption().await?;

    info!(room_id = ?alice_room.room_id(), "Alice has created and enabled encryption in the room");

    // ... and sends a message
    let event_id = alice_room
        .send(RoomMessageEventContent::text_plain("Hello Bob"))
        .await
        .expect("We should be able to send a message to the room")
        .response
        .event_id;

    // Let us create some streams to get notified about a received bundle and a
    // changed identity. We need to create now, before any action happens, so we
    // don't miss the updates.
    let bundle_stream = bob
        .encryption()
        .historic_room_key_stream()
        .await
        .expect("We should be able to get the bundle stream");

    let identity_stream = bob
        .encryption()
        .user_identities_stream()
        .await
        .expect("We should be able to get the identity stream");

    // Alice will reset her identity, once Bob sees that a reset happened, Bob will
    // mark the identity to be in a pin violation.
    info!("Alice is resetting the identity");

    let reset_future = async {
        if let Some(handle) = alice
            .encryption()
            .recovery()
            .reset_identity()
            .instrument(alice_span.clone())
            .await
            .expect("Resetting the identity should work")
        {
            handle
                .reset(Some(matrix_sdk::ruma::api::client::uiaa::AuthData::Password(
                    Password::new(
                        matrix_sdk::ruma::api::client::uiaa::UserIdentifier::UserIdOrLocalpart(
                            alice_user_id.localpart().to_owned(),
                        ),
                        alice_user_id.localpart().to_owned(),
                    ),
                )))
                .instrument(alice_span.clone())
                .await
                .expect("Providing the password to finalize the identity reset should work");
        }
    };

    timeout(reset_future, Duration::from_secs(2))
        .await
        .expect("We should be able to reset our identity");

    info!("Alice is inviting Bob");

    // Alice invites Bob to the room
    alice_room.invite_user_by_id(bob.user_id().unwrap()).instrument(alice_span.clone()).await?;

    // Alice is done. Bob has been invited and the room key bundle should have been
    // sent out. Let's stop syncing so the logs contain less noise.
    alice_sync_service.stop().await;

    // Let's wait for the bundle to arrive.
    pin_mut!(bundle_stream);
    assert_next_with_timeout!(bundle_stream, 3000);

    // Let us now wait till Alice's identity gets updated.
    info!("Bob is checking if alice's identity has changed");
    pin_mut!(identity_stream);
    let mut identity_stream = identity_stream
        .filter(|updates| future::ready(updates.changed.contains_key(alice_user_id)));
    assert_next_with_timeout!(identity_stream, 2000);

    // Let's make sure that the pin violation was recorded.
    let alice_identity = bob
        .encryption()
        .get_user_identity(alice.user_id().unwrap())
        .await?
        .expect("Bob should have access to Alice's identity");

    assert!(
        alice_identity.has_verification_violation(),
        "Alice should be in a pin violation scenario"
    );

    // Now Bob can accept the invitation.
    let room = wait_for_room(&bob, alice_room.room_id()).await;
    room.join()
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to accept the invitation from Alice");

    let event = room
        .event(&event_id, None)
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to fetch the historic event");

    // Even though we received the bundle and got notified by the stream that we
    // received it, the event should not be decryptable as we should not have
    // accepted the bundle.
    assert_matches!(
        event.kind,
        TimelineEventKind::UnableToDecrypt { .. },
        "The event should not have been decrypted, we should not receive the historic room key bundle"
    );

    Ok(())
}

/// Test history sharing where some sessions are withheld.
///
/// In this scenario we have four separate users:
///
///  1. Alice and Bob share a room, where the history visibility is set to
///    "shared".
///  2. Bob sends a message. This will be "shareable".
///  3. Alice changes the history viz to "joined".
///  4. Alice changes the history viz back to "shared", but Bob doesn't (yet)
///     receive the memo.
///  5. Bob sends a second message; the key is "unshareable" because Bob still
///     thinks the history viz is "joined".
///  6. Bob syncs, and sends a third message; the key is now "shareable".
///  7. Alice invites Charlie.
///  8. Charlie joins the room. He should see Bob's first message; the second
///     should have an appropriate withheld code from Alice; the third should be
///     decryptable.
///  9. Charlie invites Derek.
///  10. Derek joins the room, and sees the same as Charlie.
///
/// This tests correct "withheld" code handling, even with transitive invites.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_transitive_history_share_with_withhelds() -> Result<()> {
    let alice_span = tracing::info_span!("alice");
    let bob_span = tracing::info_span!("bob");
    let charlie_span = tracing::info_span!("charlie");
    let derek_span = tracing::info_span!("derek");

    let alice =
        create_encryption_enabled_client("alice", false).instrument(alice_span.clone()).await?;
    let bob = create_encryption_enabled_client("bob", false).instrument(bob_span.clone()).await?;
    let charlie =
        create_encryption_enabled_client("charlie", false).instrument(charlie_span.clone()).await?;
    let derek =
        create_encryption_enabled_client("derek", false).instrument(derek_span.clone()).await?;

    // 1. Alice creates a room, and enables encryption
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            preset: Some(RoomPreset::PublicChat),
        }))
        .instrument(alice_span.clone())
        .await?;
    let alice_timeline = alice_room.timeline().await?;

    alice_room.enable_encryption().instrument(alice_span.clone()).await?;
    // Allow regular users to send invites
    alice.sync_once().instrument(alice_span.clone()).await?;
    alice_room
        .apply_power_level_changes(RoomPowerLevelChanges { invite: Some(0), ..Default::default() })
        .instrument(alice_span.clone())
        .await
        .expect("Should be able to set power levels");

    info!(room_id = ?alice_room.room_id(), "Alice has created and enabled encryption in the room");

    // ... and invites Bob to the room
    alice_room.invite_user_by_id(bob.user_id().unwrap()).instrument(alice_span.clone()).await?;

    // Bob joins
    bob.sync_once().instrument(bob_span.clone()).await?;

    let bob_room = bob
        .join_room_by_id(alice_room.room_id())
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to accept the invitation from Alice");

    // 2. Bob sends a message, which Alice should receive
    let bob_send_test_event = async |event_content: &str| {
        let bob_event_id = bob_room
            .send(RoomMessageEventContent::text_plain(event_content))
            .into_future()
            .instrument(bob_span.clone())
            .await
            .expect("We should be able to send a message to the room")
            .response
            .event_id;

        alice
            .sync_once()
            .instrument(alice_span.clone())
            .await
            .expect("Alice should be able to sync");

        assert_event_received(&alice_timeline, &bob_event_id, event_content).await;

        bob_event_id
    };

    let event_id_1 = bob_send_test_event("Event 1").await;

    // 3. Alice changes the history visibility to "joined"
    alice_room
        .send_state_event(RoomHistoryVisibilityEventContent::new(HistoryVisibility::Joined))
        .into_future()
        .instrument(alice_span.clone())
        .await?;
    bob.sync_once().instrument(bob_span.clone()).await?;
    assert_eq!(bob_room.history_visibility(), Some(HistoryVisibility::Joined));

    // 4. Alice changes the history visibility back to "shared", but Bob doesn't
    //    know about it.
    alice_room
        .send_state_event(RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared))
        .into_future()
        .instrument(alice_span.clone())
        .await?;

    // 5. Bob sends a second message; the key is "unshareable" because Bob still
    //    thinks the history viz is "joined".
    assert_eq!(bob_room.history_visibility(), Some(HistoryVisibility::Joined));
    let event_id_2 = bob_send_test_event("Event 2").await;

    // 6. Bob syncs, and sends a third message; the key is now "shareable".
    bob.sync_once().instrument(bob_span.clone()).await?;
    assert_eq!(bob_room.history_visibility(), Some(HistoryVisibility::Shared));
    let event_id_3 = bob_send_test_event("Event 3").await;

    // 7. Alice invites Charlie.
    alice_room.invite_user_by_id(charlie.user_id().unwrap()).instrument(alice_span.clone()).await?;

    // Workaround for https://github.com/matrix-org/matrix-rust-sdk/issues/5770: Charlie needs a copy of
    // Alice's identity.
    charlie
        .encryption()
        .request_user_identity(alice.user_id().unwrap())
        .instrument(charlie_span.clone())
        .await?;

    // 8. Charlie joins the room
    charlie.sync_once().instrument(charlie_span.clone()).await?;
    let charlie_room = charlie
        .join_room_by_id(alice_room.room_id())
        .instrument(charlie_span.clone())
        .await
        .expect("Charlie should be able to accept the invitation from Alice");

    let charlie_timeline = charlie_room.timeline().await?;

    // Events 1 and 3 should be decryptable; 2 should be "history not shared".
    charlie.sync_once().instrument(charlie_span.clone()).await?;
    assert_event_received(&charlie_timeline, &event_id_1, "Event 1").await;
    assert_event_received(&charlie_timeline, &event_id_3, "Event 3").await;
    assert_utd_history_not_shared(&charlie_timeline, &event_id_2).await;

    // 9. Charlie invites Derek.
    charlie_room
        .invite_user_by_id(derek.user_id().unwrap())
        .instrument(charlie_span.clone())
        .await?;

    // Workaround for https://github.com/matrix-org/matrix-rust-sdk/issues/5770: Derek needs a copy of
    // Charlie's identity.
    derek
        .encryption()
        .request_user_identity(charlie.user_id().unwrap())
        .instrument(derek_span.clone())
        .await?;

    // 10. Derek joins the room, and sees the same as Charlie.
    derek.sync_once().instrument(derek_span.clone()).await?;
    let derek_room = derek
        .join_room_by_id(alice_room.room_id())
        .instrument(derek_span.clone())
        .await
        .expect("Derek should be able to accept the invitation from Charlie");

    let derek_timeline = derek_room.timeline().await?;

    // As for Charlie: events 1 and 3 should be decryptable; 2 should be "history
    // not shared".
    derek.sync_once().instrument(derek_span.clone()).await?;
    assert_event_received(&derek_timeline, &event_id_1, "Event 1").await;
    assert_event_received(&derek_timeline, &event_id_3, "Event 3").await;
    assert_utd_history_not_shared(&derek_timeline, &event_id_2).await;

    Ok(())
}

/// Test megolm session merging with history sharing
///
///  1. Alice and Bob share a room
///  2. Bob sends a message
///  3. Alice invites Charlie, sharing the history
///  4. Charlie can see Bob's message, but the sender is unauthenticated.
///  5. Bob sends another message (on the same session)
///  6. Charlie can now decrypt both of Bob's messages, with authenticated
///     sender.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_history_sharing_session_merging() -> Result<()> {
    let alice_span = tracing::info_span!("alice");
    let bob_span = tracing::info_span!("bob");
    let charlie_span = tracing::info_span!("charlie");

    let alice =
        create_encryption_enabled_client("alice", false).instrument(alice_span.clone()).await?;
    let bob = create_encryption_enabled_client("bob", false).instrument(bob_span.clone()).await?;
    let charlie =
        create_encryption_enabled_client("charlie", false).instrument(charlie_span.clone()).await?;

    // 1. Alice creates a room, and enables encryption
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            preset: Some(RoomPreset::PublicChat),
        }))
        .instrument(alice_span.clone())
        .await?;
    let alice_timeline = alice_room.timeline().await?;

    alice_room.enable_encryption().instrument(alice_span.clone()).await?;
    info!(room_id = ?alice_room.room_id(), "Alice has created and enabled encryption in the room");

    // ... and invites Bob to the room
    alice_room.invite_user_by_id(bob.user_id().unwrap()).instrument(alice_span.clone()).await?;

    // Bob joins
    bob.sync_once().instrument(bob_span.clone()).await?;

    let bob_room = bob
        .join_room_by_id(alice_room.room_id())
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to accept the invitation from Alice");

    // 2. Bob sends a message, which Alice should receive
    let bob_send_test_event = async |event_content: &str| {
        let bob_event_id = bob_room
            .send(RoomMessageEventContent::text_plain(event_content))
            .into_future()
            .instrument(bob_span.clone())
            .await
            .expect("We should be able to send a message to the room")
            .response
            .event_id;

        alice
            .sync_once()
            .instrument(alice_span.clone())
            .await
            .expect("Alice should be able to sync");

        assert_event_received(&alice_timeline, &bob_event_id, event_content).await;

        bob_event_id
    };

    let event_id_1 = bob_send_test_event("Event 1").await;

    // 3. Alice invites Charlie.
    alice_room.invite_user_by_id(charlie.user_id().unwrap()).instrument(alice_span.clone()).await?;

    // Workaround for https://github.com/matrix-org/matrix-rust-sdk/issues/5770: Charlie needs a copy of
    // Alice's identity.
    charlie
        .encryption()
        .request_user_identity(alice.user_id().unwrap())
        .instrument(charlie_span.clone())
        .await?;

    charlie.sync_once().instrument(charlie_span.clone()).await?;
    let charlie_room = charlie
        .join_room_by_id(alice_room.room_id())
        .instrument(charlie_span.clone())
        .await
        .expect("Charlie should be able to accept the invitation from Alice");

    // 4. Charlie can see Bob's message, but the sender is unauthenticated.
    let charlie_timeline = charlie_room.timeline().await?;
    charlie.sync_once().instrument(charlie_span.clone()).await?;
    let received_event = assert_event_received(&charlie_timeline, &event_id_1, "Event 1").await;
    assert_eq!(
        received_event
            .as_event()
            .unwrap()
            .encryption_info()
            .expect("Received event should be encrypted")
            .verification_state,
        VerificationState::Unverified(VerificationLevel::None(DeviceLinkProblem::InsecureSource))
    );

    // 5. Bob sends another message (on the same session)
    bob.sync_once().instrument(bob_span.clone()).await?;
    // Sanity: make sure Bob knows that Charlie has joined
    bob_room
        .get_member_no_sync(charlie.user_id().unwrap())
        .instrument(bob_span.clone())
        .await?
        .expect("Bob should see Charlie in the room");
    let event_id_2 = bob_send_test_event("Event 2").await;

    // 6. Charlie can now decrypt both of Bob's messages, with authenticated sender
    let mut charlie_room_stream = charlie.encryption().room_keys_received_stream().await.unwrap();
    charlie.sync_once().instrument(charlie_span.clone()).await?;

    // Make sure we're decrypting with the newly-received keys.
    assert_next_with_timeout!(&mut charlie_room_stream).expect("charlie should receive room keys");

    let received_event = assert_event_received(&charlie_timeline, &event_id_2, "Event 2").await;
    assert_eq!(
        received_event
            .as_event()
            .unwrap()
            .encryption_info()
            .expect("Received event should be encrypted")
            .verification_state,
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
    );

    // The earlier event should now have a better verification status.
    let received_event = assert_event_received(&charlie_timeline, &event_id_1, "Event 1").await;
    assert_eq!(
        received_event
            .as_event()
            .unwrap()
            .encryption_info()
            .expect("Received event should be encrypted")
            .verification_state,
        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
    );

    Ok(())
}

/// This is a very similar test to [`test_history_share_on_invite`], but we send
/// a second message once Bob has fully joined.
///
/// We can't combine this with the above since:
///
/// - We want to test that history sharing works when Alice's device is deleted,
///   which prevents Alice from sending;
/// - Sending a message after we invite Bob but before they join causes the
///   sessions to be merged, so we lose the forwarder info on the first event as
///   intended.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_history_share_on_invite_no_forwarder_info_for_normal_events() -> Result<()> {
    let alice_span = tracing::info_span!("alice");
    let bob_span = tracing::info_span!("bob");

    let alice = create_encryption_enabled_client("alice", false).await?;
    let alice_sync_service = start_client_sync_service(&alice_span, &alice).await;

    alice.encryption().wait_for_e2ee_initialization_tasks().await;
    alice_sync_service.start().await;

    let bob = create_encryption_enabled_client("bob", false).await?;

    // Alice creates a room ...
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            preset: Some(RoomPreset::PublicChat),
        }))
        .await?;
    alice_room.enable_encryption().await?;

    info!(room_id = ?alice_room.room_id(), "Alice has created and enabled encryption in the room");

    // ... and sends a message
    let event_id = alice_room
        .send(RoomMessageEventContent::text_plain("Hello Bob"))
        .await
        .expect("We should be able to send a message to the room")
        .response
        .event_id;

    let bundle_stream = bob
        .encryption()
        .historic_room_key_stream()
        .await
        .expect("We should be able to get the bundle stream");

    // Alice invites Bob to the room
    alice_room.invite_user_by_id(bob.user_id().unwrap()).await?;

    // Workaround for https://github.com/matrix-org/matrix-rust-sdk/issues/5770: Bob needs a copy of
    // Alice's identity.
    bob.encryption()
        .request_user_identity(alice.user_id().unwrap())
        .instrument(bob_span.clone())
        .await?;

    // Bob should have received a to-device event with the payload
    assert_received_room_key_bundle(bob.sync_once().instrument(bob_span.clone()).await?);

    bob.get_room(alice_room.room_id()).expect("Bob should have received the invite");

    pin_mut!(bundle_stream);

    let info = bundle_stream
        .next()
        .now_or_never()
        .flatten()
        .expect("We should be notified about the received bundle");

    assert_eq!(Some(info.sender.deref()), alice.user_id());
    assert_eq!(info.room_id, alice_room.room_id());

    let bob_room = bob
        .join_room_by_id(alice_room.room_id())
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to accept the invitation from Alice");

    let event = bob_room
        .event(&event_id, None)
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to fetch the historic event");

    assert_decrypted_message_eq!(
        event,
        "Hello Bob",
        "The decrypted event should match the message Alice has sent"
    );

    // We should be able to find the event using the high level timeline API, and
    // inspect who forwarded us the keys to decrypt.

    let alice_id = alice.user_id().unwrap();
    let alice_display_name =
        alice.account().get_display_name().await?.expect("Alice should have a display name");

    let bob_timeline = bob_room.timeline().await?;
    bob.sync_once().instrument(bob_span.clone()).await?;

    let item = assert_event_received(&bob_timeline, &event_id, "Hello Bob").await;
    let event = item.as_event().expect("The timeline item should be an event");

    assert_eq!(
        event.forwarder().expect("We should be able to access the forwarder's ID"),
        alice_id.as_str()
    );
    assert_let!(
        Some(TimelineDetails::Ready(profile)) = event.forwarder_profile(),
        "We should be able to access the forwarder's profile"
    );
    assert_eq!(
        profile
            .display_name
            .as_ref()
            .expect("We should be able to access the forwarder's display name"),
        &alice_display_name
    );

    // Alice sends a second message, which Bob should receive, but have no forwarder
    // info for as it was sent as part of a session they already have.

    let event_id = alice_room
        .send(RoomMessageEventContent::text_plain("I said Hello, Bob"))
        .await
        .expect("We should be able to send a message to the room")
        .response
        .event_id;

    bob.sync_once().instrument(bob_span.clone()).await?;

    let item = assert_event_received(&bob_timeline, &event_id, "I said Hello, Bob").await;
    assert!(
        item.as_event().expect("The timeline item should be an event").forwarder().is_none(),
        "There should be no forwarder for the second message"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_history_share_on_invite_downloads_backup_keys() -> Result<()> {
    let alice_span = tracing::info_span!("alice");
    let alice_a_span = tracing::info_span!(parent: alice_span.clone(), "device_a");
    let alice_b_span = tracing::info_span!(parent: alice_span.clone(), "device_b");
    let bob_span = tracing::info_span!("bob");

    let (alice_a, alice_b, room_id, event_id): (
        SyncTokenAwareClient,
        SyncTokenAwareClient,
        OwnedRoomId,
        OwnedEventId,
    ) = assert_can_perform_interactive_verification("alice", BackupDownloadStrategy::Manual, true)
        .instrument(alice_span)
        .await?;

    let bob = SyncTokenAwareClient::new(
        TestClientBuilder::new("bob")
            .use_sqlite()
            .encryption_settings(EncryptionSettings {
                auto_enable_cross_signing: true,
                ..Default::default()
            })
            .enable_share_history_on_invite(true)
            .build()
            .await?,
    );

    // Alice checks she can decrypt message on her first device.
    let alice_a_room = alice_a
        .get_room(&room_id)
        .expect("We should be able to fetch the room from Alice's first device");

    let alice_a_event = alice_a_room
        .event(&event_id, None)
        .instrument(alice_a_span.clone())
        .await
        .expect("Alice should be able to fetch the event on Alice's first device");

    assert!(alice_a_event.encryption_info().is_some());

    // Alice logs out from her first device.
    alice_a.logout().instrument(alice_a_span.clone()).await?;
    alice_b.sync_once().instrument(alice_b_span.clone()).await?;

    // Alice attempts to decrypt the message on her second device, which should fail
    // as she has not downloaded the key from her backup.
    let alice_b_room = alice_b
        .get_room(&room_id)
        .expect("We should be able to fetch the room from Alice's second device");

    let alice_b_event = alice_b_room
        .event(&event_id, None)
        .instrument(alice_b_span.clone())
        .await
        .expect("Alice should be able to fetch the event from Alice's second device");

    assert!(
        alice_b_event.encryption_info().is_none(),
        "Alice was able to decrypt the event before inviting Bob"
    );

    let bundle_stream = bob
        .encryption()
        .historic_room_key_stream()
        .await
        .expect("We should be able to get the bundle stream");

    // Alice now invites Bob to the room ...
    alice_b.sync_once().instrument(alice_b_span.clone()).await?;
    alice_b_room
        .invite_user_by_id(bob.user_id().expect("We should be able to compute Bob's ID"))
        .instrument(alice_b_span.clone())
        .await?;

    // ... which should trigger a download from key backup, allowing her to decrypt
    // the message.

    let alice_b_event = alice_b_room
        .event(&event_id, None)
        .instrument(alice_b_span.clone())
        .await
        .expect("Alice should be able to fetch the event");

    assert!(
        alice_b_event.encryption_info().is_some(),
        "Alice was not able to decrypt the event after inviting Bob"
    );

    // Alice is done, let's log her out.
    alice_b.logout().instrument(alice_b_span.clone()).await?;

    // Workaround for https://github.com/matrix-org/matrix-rust-sdk/issues/5770: Bob needs a copy of
    // Alice's identity.
    bob.encryption()
        .request_user_identity(alice_b.user_id().unwrap())
        .instrument(bob_span.clone())
        .await?;

    // Bob joins the room ...
    let bob_response = bob.sync_once().instrument(bob_span.clone()).await?;

    // ... and checks he received a to-device event with the payload.
    assert_received_room_key_bundle(bob_response);

    bob.get_room(&room_id).expect("Bob should have received the invite");

    pin_mut!(bundle_stream);

    let info = bundle_stream
        .next()
        .now_or_never()
        .flatten()
        .expect("We should be notified about the received bundle");

    assert_eq!(Some(info.sender.deref()), alice_b.user_id());
    assert_eq!(info.room_id, room_id);

    // We now check that Bob can access the event.
    let bob_room = bob
        .join_room_by_id(&room_id)
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to accept the invitation from Alice");

    let bob_event = bob_room
        .event(&event_id, None)
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to fetch the historic event");

    assert!(bob_event.encryption_info().is_some());

    Ok(())
}

/// Creates a new encryption-enabled client with the given username and
/// settings.
///
/// # Arguments
///
/// * `username` - The username for the client.
/// * `exclude_insecure_devices` - A boolean indicating whether to exclude
///   insecure devices.
async fn create_encryption_enabled_client(
    username: &str,
    exclude_insecure_devices: bool,
) -> Result<SyncTokenAwareClient> {
    let encryption_settings =
        EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

    let client = SyncTokenAwareClient::new(
        TestClientBuilder::new(username)
            .use_sqlite()
            .encryption_settings(encryption_settings)
            .enable_share_history_on_invite(true)
            .exclude_insecure_devices(exclude_insecure_devices)
            .build()
            .await?,
    );

    client.encryption().wait_for_e2ee_initialization_tasks().await;
    Ok(client)
}

/**
 * Wait for an event with the given ID to appear in the timeline.
 *
 * This function will wait for an event to appear in the timeline, and then
 * return it. If the event doesn't appear within the given timeout, it will
 * return `None`.
 */
async fn wait_for_timeline_event(
    timeline: &Timeline,
    event_id: &EventId,
) -> Option<Arc<TimelineItem>> {
    let predicate =
        |item: &Arc<TimelineItem>| item.as_event().and_then(|e| e.event_id()) == Some(event_id);

    let (items, stream) = timeline.subscribe().await;

    // If a matching event is already in the timeline, return it.
    if let Some(event) = items.into_iter().find(|item| predicate(item)) {
        return Some(event);
    }

    // Otherwise, wait for it to arrive.
    pin_mut!(stream);

    loop {
        let diffs = match tokio::time::timeout(Duration::from_millis(500), stream.next()).await {
            Err(_) => return None, // We timed out while waiting for an event to arrive.
            Ok(None) => panic!("Stream ended unexpectedly"),
            Ok(Some(diffs)) => diffs,
        };

        for diff in diffs {
            let matched_event = match diff {
                VectorDiff::Append { values } | VectorDiff::Reset { values } => {
                    values.into_iter().find(predicate)
                }
                VectorDiff::PushBack { value }
                | VectorDiff::PushFront { value }
                | VectorDiff::Insert { value, .. }
                | VectorDiff::Set { value, .. } => predicate(&value).then_some(value),
                _ => None,
            };

            if let Some(event) = matched_event {
                return Some(event);
            }
        }
    }
}

/**
 * Wait for the given event to arrive in the timeline, and assert that its
 * content matches that given.
 */
async fn assert_event_received(
    timeline: &Timeline,
    event_id: &EventId,
    expected_content: &str,
) -> Arc<TimelineItem> {
    let timeline_item = wait_for_timeline_event(timeline, event_id).await.unwrap_or_else(|| {
        panic!("Timeout waiting for event {event_id} with content {expected_content} to arrive")
    });

    assert_let!(
        TimelineItemContent::MsgLike(msg_like_content) =
            timeline_item.as_event().unwrap().content()
    );
    assert_let!(MsgLikeContent { kind: MsgLikeKind::Message(message), .. } = msg_like_content);
    assert_eq!(
        message.body(),
        expected_content,
        "The decrypted event should match the message Bob has sent"
    );

    timeline_item
}

/**
 * Assert that the given event is a UTD, with a withheld code of
 * "history_not_shared", and an appropriate UtdCause.
 */
async fn assert_utd_history_not_shared(timeline: &Timeline, event_id: &EventId) {
    let timeline_item = wait_for_timeline_event(timeline, event_id)
        .await
        .unwrap_or_else(|| panic!("Timeout waiting for Bob's withheld event {event_id} to arrive"));

    assert_let!(
        TimelineItemContent::MsgLike(msg_like_content) =
            timeline_item.as_event().unwrap().content()
    );
    assert_let!(
        MsgLikeContent { kind: MsgLikeKind::UnableToDecrypt(encrypted), .. } = msg_like_content
    );
    assert_let!(EncryptedMessage::MegolmV1AesSha2 { cause, .. } = encrypted);
    // It should be reported in the UI as a regular "You don't have access to this
    // event".
    assert_eq!(*cause, UtdCause::SentBeforeWeJoined);

    // The timeline interface doesn't expose the raw withheld code, so call
    // `Room::event` to find it.
    let event =
        timeline.room().event(event_id, None).await.expect("Should receive Bob's withheld event");
    assert_let!(TimelineEventKind::UnableToDecrypt { utd_info, .. } = event.kind);
    assert_eq!(
        utd_info.reason,
        MissingMegolmSession { withheld_code: Some(WithheldCode::HistoryNotShared) }
    );
}

/// Asserts that the given `sync_response` contains exactly one to-device event
/// and that the event is a decrypted room key bundle.
fn assert_received_room_key_bundle(sync_response: matrix_sdk::sync::SyncResponse) {
    assert_eq!(sync_response.to_device.len(), 1, "Expected exactly one to-device event");
    let to_device_event = &sync_response.to_device[0];
    assert_let!(
        ProcessedToDeviceEvent::Decrypted { raw, .. } = to_device_event,
        "Expected the to-device event to be decrypted"
    );
    assert_eq!(
        raw.get_field::<String>("type").unwrap().unwrap(),
        "io.element.msc4268.room_key_bundle",
        "Expected the event type to be 'io.element.msc4268.room_key_bundle'"
    );
}

/// Start the given client's sync service and attach a new span to track logs.
async fn start_client_sync_service(
    span: &tracing::Span,
    client: &impl Deref<Target = Client>,
) -> SyncService {
    let sync_service_span = tracing::info_span!(parent: span, "sync_service");
    let sync_service = SyncService::builder(client.deref().clone())
        .with_parent_span(sync_service_span)
        .build()
        .await
        .expect("Could not build sync service");

    client.encryption().wait_for_e2ee_initialization_tasks().await;
    sync_service.start().await;
    sync_service
}
