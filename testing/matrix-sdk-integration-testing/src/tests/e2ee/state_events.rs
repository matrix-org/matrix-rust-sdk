use std::{ops::Deref, time::Duration};

use anyhow::Result;
use assert_matches2::assert_let;
use assign::assign;
use futures::{FutureExt, StreamExt, pin_mut};
use matrix_sdk::{
    assert_let_decrypted_state_event_content,
    encryption::EncryptionSettings,
    ruma::{
        api::client::room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        events::AnyStateEventContent,
    },
};
use matrix_sdk_common::deserialized_responses::ProcessedToDeviceEvent;
use matrix_sdk_ui::sync_service::SyncService;
use similar_asserts::assert_eq;
use tracing::{Instrument, info};

use crate::helpers::{SyncTokenAwareClient, TestClientBuilder};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_e2ee_state_events() -> Result<()> {
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

    let sync_service_span = tracing::info_span!(parent: &alice_span, "sync_service");
    let alice_sync_service = SyncService::builder(alice.clone())
        .with_parent_span(sync_service_span)
        .build()
        .await
        .expect("Could not build alice sync service");

    alice.encryption().wait_for_e2ee_initialization_tasks().await;
    alice_sync_service.start().await;

    let bob = SyncTokenAwareClient::new(
        TestClientBuilder::new("bob")
            .encryption_settings(encryption_settings)
            .enable_share_history_on_invite(true)
            .build()
            .await?,
    );

    // Alice creates an encrypted room ...
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            name: Some("Cat Photos".to_owned()),
            preset: Some(RoomPreset::PublicChat),
        }))
        .await?;
    alice_room.enable_encryption_with_state_event_encryption().await?;

    // HACK: wait for sync
    let _ = tokio::time::sleep(Duration::from_secs(3)).await;

    // (sanity checks)
    assert!(alice_room.encryption_state().is_encrypted(), "Encryption was not enabled.");
    assert!(
        alice_room.encryption_state().is_state_encrypted(),
        "State encryption was not enabled."
    );

    info!(room_id = ?alice_room.room_id(), "Alice has created and enabled encryption in the room");

    // ... and changes the room name
    let rename_event_id = alice_room
        .set_name("Dog Photos".to_owned())
        .await
        .expect("We should be able to rename the room")
        .event_id;

    let bundle_stream = bob
        .encryption()
        .historic_room_key_stream()
        .await
        .expect("We should be able to get the bundle stream");

    // Alice invites Bob to the room
    alice_room.invite_user_by_id(bob.user_id().unwrap()).await?;

    // Alice is done. Bob has been invited and the room key bundle should have been
    // sent out. Let's stop syncing so the logs contain less noise.
    alice_sync_service.stop().await;

    let bob_response = bob.sync_once().instrument(bob_span.clone()).await?;

    // Bob should have received a to-device event with the payload
    assert_eq!(bob_response.to_device.len(), 1);
    let to_device_event = &bob_response.to_device[0];
    assert_let!(ProcessedToDeviceEvent::Decrypted { raw, .. } = to_device_event);
    assert_eq!(
        raw.get_field::<String>("type").unwrap().unwrap(),
        "io.element.msc4268.room_key_bundle"
    );

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

    // Sync the room, so the rename event arrives.
    let _ = bob.sync_once().instrument(bob_span.clone()).await?;

    // Check it has been applied.
    assert_eq!(
        "Dog Photos",
        bob_room.name().unwrap(),
        "Bob's copy of the room name should have updated."
    );

    // Let's also check we can inspect the payload manually.
    let rename_event = bob_room
        .event(&rename_event_id, None)
        .instrument(bob_span.clone())
        .await
        .expect("Bob should be able to fetch the historic event.");

    assert_let_decrypted_state_event_content!(
        AnyStateEventContent::RoomName(content) = rename_event
    );

    assert_eq!("Dog Photos", content.name);

    Ok(())
}
