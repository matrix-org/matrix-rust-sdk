use std::ops::Not;

use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{events::tag::TagName, room_id};

#[async_test]
async fn test_set_favourite() {
    let room_id = room_id!("!test:example.org");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    let f = EventFactory::new().room(room_id);

    // Not favourite.
    assert!(room.is_favourite().not());

    // Server will be called to set the room as favourite.
    server.mock_set_tag(&TagName::Favorite).ok().mock_once().mount().await;

    room.set_is_favourite(true, None).await.unwrap();

    // Mock the response from the server.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_account_data(f.tag_from_names(vec![TagName::Favorite])),
        )
        .await;

    // Favourite!
    assert!(room.is_favourite());
}

#[async_test]
async fn test_set_favourite_on_low_priority_room() {
    let room_id = room_id!("!test:example.org");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().room(room_id);

    // Mock a response from the server setting the room as low priority.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_account_data(f.tag_from_names(vec![TagName::LowPriority])),
        )
        .await;

    // Not favourite, but low priority!
    assert!(room.is_favourite().not());
    assert!(room.is_low_priority());

    // Server will be called to set the room as favourite, and to unset the room
    // as low priority.
    server.mock_set_tag(&TagName::Favorite).ok().mock_once().mount().await;
    server.mock_delete_tag(&TagName::LowPriority).ok().mock_once().mount().await;

    room.set_is_favourite(true, None).await.unwrap();

    // Mock the response from the server.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_account_data(f.tag_from_names(vec![TagName::Favorite])),
        )
        .await;

    // Favourite, and not low priority!
    assert!(room.is_favourite());
    assert!(room.is_low_priority().not());
}

#[async_test]
async fn test_unset_favourite() {
    let room_id = room_id!("!test:example.org");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;

    // Server will be called to unset the room as favourite.
    server.mock_delete_tag(&TagName::Favorite).ok().mock_once().mount().await;

    room.set_is_favourite(false, None).await.unwrap();
}

#[async_test]
async fn test_set_low_priority() {
    let room_id = room_id!("!test:example.org");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;
    let f = EventFactory::new().room(room_id);

    // Not low prioriry.
    assert!(room.is_low_priority().not());

    // Server will be called to set the room as low priority.
    server.mock_set_tag(&TagName::LowPriority).ok().mock_once().mount().await;

    room.set_is_low_priority(true, None).await.unwrap();

    // Mock the response from the server.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_account_data(f.tag_from_names(vec![TagName::LowPriority])),
        )
        .await;

    // Low priority!
    assert!(room.is_low_priority());
}

#[async_test]
async fn test_set_low_priority_on_favourite_room() {
    let room_id = room_id!("!test:example.org");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().room(room_id);

    // Mock a response from the server setting the room as favourite.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_account_data(f.tag_from_names(vec![TagName::Favorite])),
        )
        .await;

    // Favourite, but not low priority!
    assert!(room.is_favourite());
    assert!(room.is_low_priority().not());

    // Server will be called to set the room as favourite, and to unset the room
    // as low priority.
    server.mock_set_tag(&TagName::LowPriority).ok().mock_once().mount().await;
    server.mock_delete_tag(&TagName::Favorite).ok().mock_once().mount().await;

    room.set_is_low_priority(true, None).await.unwrap();

    // Mock the response from the server.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_account_data(f.tag_from_names(vec![TagName::LowPriority])),
        )
        .await;

    // Not favourite, and low priority!
    assert!(room.is_favourite().not());
    assert!(room.is_low_priority());
}

#[async_test]
async fn test_unset_low_priority() {
    let room_id = room_id!("!test:example.org");
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_joined_room(&client, room_id).await;

    // Server will be called to unset the room as favourite.
    server.mock_delete_tag(&TagName::LowPriority).ok().mock_once().mount().await;

    room.set_is_low_priority(false, None).await.unwrap();
}
