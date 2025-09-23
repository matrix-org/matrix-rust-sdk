use std::sync::{Arc, Mutex};

use assert_matches2::assert_matches;
use matrix_sdk::{room::calls::CallError, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, StateTestEvent, async_test, event_factory::EventFactory};
use ruma::{
    OwnedUserId, events::rtc::notification::NotificationType, owned_event_id, room_id, user_id,
};
use tokio::spawn;

#[async_test]
async fn test_subscribe_to_decline_call_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let decliners_sequences: Arc<Mutex<Vec<OwnedUserId>>> = Arc::new(Mutex::new(Vec::new()));
    let asserted_decliners = vec![user_id!("@bob:matrix.org"), user_id!("@carl:example.com")];

    let room_id = room_id!("!test:example.org");

    let notification_event_id = owned_event_id!("$00000:localhost");

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.rtc_notification(NotificationType::Ring)
                    .sender(user_id!("@alice:matrix.org"))
                    .event_id(&notification_event_id),
            ),
        )
        .await;

    let room = server.sync_joined_room(&client, room_id).await;

    let to_subscribe_to = notification_event_id.clone();
    let join_handle = spawn({
        let decliners_sequences = Arc::clone(&decliners_sequences);
        async move {
            let (_drop_guard, mut subscriber) =
                room.subscribe_to_call_decline_events(&to_subscribe_to);

            while let Ok(user_id) = subscriber.recv().await {
                let mut decliners_sequences = decliners_sequences.lock().unwrap();
                decliners_sequences.push(user_id);

                // When we have received 2 typing notifications, we can stop listening.
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
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    // declines another call
                    f.call_decline(&other_notification_event_id)
                        .sender(user_id!("@valere:matrix.org")),
                )
                .add_timeline_event(
                    // declines another call
                    f.call_decline(&notification_event_id).sender(user_id!("@bob:matrix.org")),
                )
                .add_timeline_event(
                    // declines another call
                    f.call_decline(&notification_event_id).sender(user_id!("@carl:example.com")),
                ),
        )
        .await;

    join_handle.await.unwrap();
    assert_eq!(decliners_sequences.lock().unwrap().to_vec(), asserted_decliners);
}

#[async_test]
async fn test_decline_call() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    println!("client id is: {}", client.user_id().unwrap());
    let room_id = room_id!("!test:example.org");

    let notification_event_id = owned_event_id!("$00000:localhost");
    let a_message_event_id = owned_event_id!("$00001:localhost");
    let unknown_event_id = owned_event_id!("$00002:localhost");
    let own_notification_event_id = owned_event_id!("$00003:localhost");

    // Subscribe to the event cache (to avoid having to remotely fetch the related
    // event)
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_event(StateTestEvent::Encryption)
                .add_timeline_event(
                    f.rtc_notification(NotificationType::Ring)
                        .sender(user_id!("@alice:matrix.org"))
                        .event_id(&notification_event_id),
                )
                .add_timeline_event(
                    f.rtc_notification(NotificationType::Ring)
                        .sender(user_id!("@example:localhost"))
                        .event_id(&own_notification_event_id),
                )
                .add_timeline_event(
                    f.text_msg("Hello, HRU? ")
                        .event_id(&a_message_event_id)
                        .sender(user_id!("@alice:matrix.org")),
                ),
        )
        .await;

    let room = server.sync_joined_room(&client, room_id).await;

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
