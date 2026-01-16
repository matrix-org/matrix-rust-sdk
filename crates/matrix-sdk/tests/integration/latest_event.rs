use assert_matches::assert_matches;
use matrix_sdk::{
    latest_events::LatestEventValue,
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{async_test, event_factory::EventFactory};
use ruma::{event_id, room_id, user_id};
use tokio::task::yield_now;

#[async_test]
async fn test_latest_event_is_recomputed_when_a_user_is_ignored() {
    let room_id = room_id!("!r0").to_owned();
    let alice = user_id!("@alice:local");
    let bob = user_id!("@bob:local");
    let event_alice = event_id!("$ev0");
    let event_bob = event_id!("$ev1");
    let event_factory = EventFactory::new().room(&room_id);

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    // Fill the event cache with one event.
    client
        .event_cache_store()
        .lock()
        .await
        .expect("Could not acquire the event cache lock")
        .as_clean()
        .expect("Could not acquire a clean event cache lock")
        .handle_linked_chunk_updates(
            LinkedChunkId::Room(&room_id),
            vec![
                Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(0), next: None },
                Update::PushItems {
                    at: Position::new(ChunkIdentifier::new(0), 0),
                    items: vec![
                        event_factory.text_msg("A").sender(alice).event_id(event_alice).into(),
                        event_factory.text_msg("B").sender(bob).event_id(event_bob).into(),
                    ],
                },
            ],
        )
        .await
        .unwrap();

    // Create the room.
    let _room = server.sync_joined_room(&client, &room_id).await;

    // Listen to the latest event to get updates.
    let mut latest_event_stream =
        client.latest_events().await.listen_and_subscribe_to_room(&room_id).await.unwrap().unwrap();

    // In some configurations, this makes the test non-flaky.
    yield_now().await;

    // The latest event is the one from Bob.
    assert_matches!(
        latest_event_stream.next_now().await,
        LatestEventValue::Remote(event) => {
            assert_eq!(event.event_id().as_deref(), Some(event_bob));
        }
    );

    // Let's ignore Bob now.
    server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_global_account_data(event_factory.ignored_user_list([bob.to_owned()]));
        })
        .await;

    // In some configurations, this makes the test non-flaky.
    yield_now().await;

    // The latest event is reset to `None` because the room has been cleared.
    assert_matches!(latest_event_stream.next().await, Some(LatestEventValue::None));
}
