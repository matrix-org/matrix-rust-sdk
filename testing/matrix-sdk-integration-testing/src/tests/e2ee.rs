use std::{
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Result;
use assign::assign;
use matrix_sdk::ruma::{
    api::client::room::create_room::v3::Request as CreateRoomRequest,
    events::room::message::{MessageType, RoomMessageEventContent, SyncRoomMessageEvent},
};
use tracing::warn;

use crate::helpers::get_sync_aware_client_for_user;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_encryption_missing_member_keys() -> Result<()> {
    let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    let alice = get_sync_aware_client_for_user(format!("alice{time}")).await?;
    let bob = get_sync_aware_client_for_user(format!("bob{time}")).await?;

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
    let carl = get_sync_aware_client_for_user(format!("carl{time}").to_owned()).await?;
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
    bob_room.send(RoomMessageEventContent::text_plain(message), None).await?;
    warn!("bob is done sending the message");

    // Alice was in the room when Bob sent the message, so they'll see it.
    let alice_found_event = Arc::new(Mutex::new(false));
    {
        warn!("alice is looking for decrypted message");

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
        warn!("carl is looking for decrypted message");

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
    bob_room.send(RoomMessageEventContent::text_plain(message), None).await?;
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
    let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    let alice = get_sync_aware_client_for_user(format!("alice{time}")).await?;
    let bob = get_sync_aware_client_for_user(format!("bob{time}")).await?;

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
    bob_room.send(RoomMessageEventContent::text_plain(message), None).await?;
    warn!("bob is done sending the message");

    // Alice sees the message.
    let alice_found_event = Arc::new(Mutex::new(false));
    warn!("alice is looking for decrypted message");

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
