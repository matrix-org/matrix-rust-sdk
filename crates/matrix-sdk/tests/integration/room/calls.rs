use std::sync::{Arc, Mutex};

use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_test::{async_test, event_factory::EventFactory, JoinedRoomBuilder};
use ruma::{owned_event_id, room_id, user_id, OwnedUserId};
use tokio::spawn;

#[async_test]
async fn test_subscribe_to_decline_call_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let decliners_sequences: Arc<Mutex<Vec<OwnedUserId>>> = Arc::new(Mutex::new(Vec::new()));
    let asserted_decliners = vec![user_id!("@bob:matrix.org"), user_id!("@carl:example.com")];

    let room_id = room_id!("!test:example.org");

    let notification_event_id = owned_event_id!("$00000:localhost");
    // Initial sync with our test room.
    // let room = server.sync_joined_room(&client, room_id).await;

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.call_notify(
                    "call_id".to_owned(),
                    ruma::events::call::notify::ApplicationType::Call,
                    ruma::events::call::notify::NotifyType::Ring,
                    ruma::events::Mentions::new(),
                )
                .sender(&user_id!("@alice:matrix.org"))
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
                        .sender(&user_id!("@valere:matrix.org")),
                )
                .add_timeline_event(
                    // declines another call
                    f.call_decline(&notification_event_id).sender(&user_id!("@bob:matrix.org")),
                )
                .add_timeline_event(
                    // declines another call
                    f.call_decline(&notification_event_id).sender(&user_id!("@carl:example.com")),
                ),
        )
        .await;

    join_handle.await.unwrap();
    assert_eq!(decliners_sequences.lock().unwrap().to_vec(), asserted_decliners);
}
