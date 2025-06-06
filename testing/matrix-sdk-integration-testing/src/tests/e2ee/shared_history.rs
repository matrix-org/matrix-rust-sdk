use anyhow::Result;
use assert_matches2::assert_let;
use assign::assign;
use matrix_sdk::{
    assert_decrypted_message_eq,
    encryption::EncryptionSettings,
    ruma::{
        api::client::room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        events::room::message::RoomMessageEventContent,
    },
};
use matrix_sdk_common::deserialized_responses::ProcessedToDeviceEvent;
use matrix_sdk_ui::sync_service::SyncService;
use similar_asserts::assert_eq;
use tracing::{info, Instrument};

use crate::helpers::{SyncTokenAwareClient, TestClientBuilder};

/// When we invite another user to a room with "joined" history visibility, we
/// share the encryption history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_history_share_on_invite() -> Result<()> {
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
        .event_id;

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

    let bob_room = bob.get_room(alice_room.room_id()).expect("Bob should have received the invite");

    bob_room
        .join()
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

    Ok(())
}
