use std::{sync::Arc, time::Duration};

use anyhow::Result;
use assign::assign;
use matrix_sdk::{
    crypto::UserDevices,
    encryption::EncryptionSettings,
    ruma::{
        api::client::room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        events::room::message::RoomMessageEventContent,
        UserId,
    },
    timeout::timeout,
    Client,
};
use matrix_sdk_ui::sync_service::SyncService;
use similar_asserts::assert_eq;
use tracing::{info, Instrument};

use crate::helpers::{wait_until_some, SyncTokenAwareClient, TestClientBuilder};

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
        .build()
        .await?;

    let sync_service_span = tracing::info_span!(parent: &alice_span, "sync_service");
    let alice_sync_service = Arc::new(
        SyncService::builder(alice.clone())
            .with_parent_span(sync_service_span)
            .build()
            .await
            .expect("Could not build alice sync service"),
    );

    alice.encryption().wait_for_e2ee_initialization_tasks().await;
    alice_sync_service.start().await;

    let bob = SyncTokenAwareClient::new(
        TestClientBuilder::new("bob").encryption_settings(encryption_settings).build().await?,
    );

    {
        // Alice and Bob share an encrypted room
        // TODO: get rid of all of this: history sharing should work even if Bob and
        //   Alice do not share a room
        let alice_shared_room = alice
            .create_room(assign!(CreateRoomRequest::new(), {preset: Some(RoomPreset::PublicChat)}))
            .await?;
        let shared_room_id = alice_shared_room.room_id();
        alice_shared_room.enable_encryption().await?;
        bob.join_room_by_id(shared_room_id)
            .instrument(bob_span.clone())
            .await
            .expect("Bob should have joined the room");

        // Alice should now have a pending `/keys/query` request for Bob, but we need
        // her to actually send it, which we do by having her wait for Bob to join, and
        // then send an encrypted message.
        //
        // (The `room-list` sync loop will register Bob's keys as out of date, but that
        // loop doesn't handle pending outgoing crypto requests. So an alternative would
        // be to get the `encryption` sync loop to cycle, but this is more direct.)

        wait_until_some(
            async |_| alice_shared_room.get_member(bob.user_id().unwrap()).await.unwrap(),
            Duration::from_secs(30),
        )
        .await
        .expect("Alice did not see bob in room");

        alice_shared_room
            .send(RoomMessageEventContent::text_plain(""))
            .await
            .expect("Alice could not send message in room");

        // Sanity check: Both users see the others' device
        async fn devices_seen(client: &Client, other: &UserId) -> UserDevices {
            client
                .olm_machine_for_testing()
                .await
                .as_ref()
                .unwrap()
                .get_user_devices(other, Some(Duration::from_secs(1)))
                .await
                .unwrap()
        }

        timeout(
            async {
                loop {
                    let bob_devices = devices_seen(&alice, bob.user_id().unwrap()).await;
                    if bob_devices.devices().count() >= 1 {
                        return;
                    }
                }
            },
            Duration::from_secs(30), // This can take quite a while to happen on the CI runners.
        )
        .await
        .expect("Alice did not see bob's device");

        bob.sync_once().instrument(bob_span.clone()).await?;
        let alice_devices = devices_seen(&bob, alice.user_id().unwrap()).await;
        assert_eq!(alice_devices.devices().count(), 1, "Bob did not see Alice's device");
    }

    // Alice creates a room ...
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            preset: Some(RoomPreset::PublicChat),
        }))
        .await?;
    alice_room.enable_encryption().await?;

    info!(room_id = ?alice_room.room_id(), "Alice has created and enabled encryption in the room");

    // ... and sends a message
    alice_room
        .send(RoomMessageEventContent::text_plain("Hello Bob"))
        .await
        .expect("We should be able to send a message to the room");

    // Alice invites Bob to the room
    alice_room.invite_user_by_id(bob.user_id().unwrap()).await?;

    let bob_response = bob.sync_once().instrument(bob_span.clone()).await?;

    // Bob should have received a to-device event with the payload
    assert_eq!(bob_response.to_device.len(), 1);
    let to_device_event = &bob_response.to_device[0];
    assert_eq!(
        to_device_event.get_field::<String>("type").unwrap().unwrap(),
        "io.element.msc4268.room_key_bundle"
    );

    // TODO: ensure Bob can decrypt the content

    Ok(())
}
