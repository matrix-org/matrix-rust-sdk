use std::{
    assert_matches,
    sync::{Arc, Mutex},
};

use matrix_sdk::{room::calls::CallError, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, User, async_test, event_factory::EventFactory};
use ruma::{OwnedUserId, events::rtc::notification::NotificationType, owned_event_id};
use tokio::spawn;

#[async_test]
async fn test_subscribe_to_decline_call_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let decliners_sequences: Arc<Mutex<Vec<OwnedUserId>>> = Arc::new(Mutex::new(Vec::new()));
    let asserted_decliners = [User::Bob, User::Carol];

    let notification_event_id = owned_event_id!("$00000:localhost");

    let f = EventFactory::new();
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_event(
                f.rtc_notification(NotificationType::Ring)
                    .sender(User::Alice)
                    .event_id(&notification_event_id),
            ),
        )
        .await;

    let to_subscribe_to = notification_event_id.clone();
    let join_handle = spawn({
        let decliners_sequences = Arc::clone(&decliners_sequences);
        async move {
            let (_drop_guard, mut subscriber) =
                room.subscribe_to_call_decline_events(&to_subscribe_to);

            while let Ok(user_id) = subscriber.recv().await {
                let mut decliners_sequences = decliners_sequences.lock().unwrap();
                decliners_sequences.push(user_id);

                // When we have received 2 typing notifications, we can stop
                // listening.
                if decliners_sequences.len() == 2 {
                    break;
                }
            }
        }
    });

    let other_notification_event_id = owned_event_id!("$11111:localhost");

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(
                    // declines another call
                    f.call_decline(&other_notification_event_id).sender(User::Ella),
                )
                .add_timeline_event(
                    // declines another call
                    f.call_decline(&notification_event_id).sender(User::Bob),
                )
                .add_timeline_event(
                    // declines another call
                    f.call_decline(&notification_event_id).sender(User::Carol),
                ),
        )
        .await;

    join_handle.await.unwrap();
    assert_eq!(*decliners_sequences.lock().unwrap(), asserted_decliners);
}

#[async_test]
async fn test_decline_call() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    println!("client id is: {user_id}");

    let notification_event_id = owned_event_id!("$00000:localhost");
    let a_message_event_id = owned_event_id!("$00001:localhost");
    let unknown_event_id = owned_event_id!("$00002:localhost");
    let own_notification_event_id = owned_event_id!("$00003:localhost");

    let f = EventFactory::new().sender(User::Alice);
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_event(f.room_encryption())
                .add_timeline_event(
                    f.rtc_notification(NotificationType::Ring).event_id(&notification_event_id),
                )
                .add_timeline_event(
                    f.rtc_notification(NotificationType::Ring)
                        .sender(user_id)
                        .event_id(&own_notification_event_id),
                )
                .add_timeline_event(f.text_msg("Hello, HRU? ").event_id(&a_message_event_id)),
        )
        .await;

    let room = server.sync_default_joined_room(&client).await;

    // Declining an unknown call should fail
    let result = room.make_decline_call_event(&unknown_event_id).await;
    assert!(result.is_err());
    assert_matches!(result.unwrap_err(), CallError::Fetch(_));

    // Declining a non call notification event should fail
    let result = room.make_decline_call_event(&a_message_event_id).await;
    assert!(result.is_err());
    assert_matches!(result.unwrap_err(), CallError::BadEventType);

    // Declining your own call notification event should fail
    let result = room.make_decline_call_event(&own_notification_event_id).await;
    assert!(result.is_err());
    assert_matches!(result.unwrap_err(), CallError::DeclineOwnCall);

    let event =
        room.make_decline_call_event(&notification_event_id).await.expect("Should not fail");

    // try to queue
    room.send_queue().send(event.into()).await.expect("Should not fail");
}
