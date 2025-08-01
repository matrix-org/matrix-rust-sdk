use assert_matches2::assert_matches;
use matrix_sdk::{room::ThreadStatus, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::async_test;
use ruma::{owned_event_id, room_id};

#[async_test]
async fn test_subscribe_thread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!test:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    let root_id = owned_event_id!("$root");

    server
        .mock_put_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(root_id.clone())
        .ok()
        .mock_once()
        .mount()
        .await;

    // I can subscribe to a thread.
    room.subscribe_thread(root_id.clone(), true).await.unwrap();

    server
        .mock_get_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(root_id.clone())
        .ok(true)
        .mock_once()
        .mount()
        .await;

    // I can get the subscription status for that same thread.
    let subscription = room.fetch_thread_subscription(root_id.clone()).await.unwrap().unwrap();
    assert_matches!(subscription, ThreadStatus::Subscribed { automatic: true });

    // If I try to get a subscription for a thread event that's unknown, I get no
    // `ThreadSubscription`, not an error.
    let subscription =
        room.fetch_thread_subscription(owned_event_id!("$another_root")).await.unwrap();
    assert!(subscription.is_none());

    // I can also unsubscribe from a thread.
    server
        .mock_delete_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(root_id.clone())
        .ok()
        .mock_once()
        .mount()
        .await;

    room.unsubscribe_thread(root_id.clone()).await.unwrap();

    // Now, if I retry to get the subscription status for this thread, it's
    // unsubscribed.
    let subscription = room.fetch_thread_subscription(root_id).await.unwrap();
    assert_matches!(subscription, Some(ThreadStatus::Unsubscribed));
}
