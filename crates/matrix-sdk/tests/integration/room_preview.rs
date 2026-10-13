use assert_matches::assert_matches;
use js_int::uint;
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_base::{RequestedRequiredStates, RoomState};
use matrix_sdk_test::{InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, async_test};
use ruma::{
    api::client::sync::sync_events::v5::{self as sliding_sync_http, response::Hero},
    assign,
    events::room::member::MembershipState,
    owned_user_id, room_id,
};

#[async_test]
async fn test_room_preview_leave_invited() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    let room = server.sync_room(&client, InvitedRoomBuilder::new(room_id)).await;

    server
        .mock_room_summary()
        .ok_with_membership(room_id, MembershipState::Invite)
        .mock_once()
        .mount()
        .await;
    server.mock_room_leave().ok(room_id).mock_once().mount().await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Invited);

    room.leave().await.unwrap();

    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_invite_leave_room_summary_msc3266_disabled() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    let room = server.sync_room(&client, InvitedRoomBuilder::new(room_id)).await;

    // A preview should be built from the sync data above
    let preview = client
        .get_room_preview(room_id.into(), Vec::new())
        .await
        .expect("Room preview should be retrieved");

    assert_eq!(preview.room_id, room_id);
    assert_matches!(preview.state.unwrap(), RoomState::Invited);

    server.mock_room_leave().ok(room_id).expect(1).mount().await;

    room.leave().await.unwrap();

    assert_matches!(room.state(), RoomState::Left);
    assert_matches!(
        client.get_room_preview(room_id.into(), Vec::new()).await.unwrap().state.unwrap(),
        RoomState::Left
    );
}

#[async_test]
async fn test_room_preview_leave_knocked() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    let room = server.sync_room(&client, KnockedRoomBuilder::new(room_id)).await;

    server
        .mock_room_summary()
        .ok_with_membership(room_id, MembershipState::Knock)
        .mock_once()
        .mount()
        .await;
    server.mock_room_leave().ok(room_id).mock_once().mount().await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Knocked);

    room.leave().await.unwrap();

    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_leave_joined() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    let room = server.sync_room(&client, JoinedRoomBuilder::new(room_id)).await;

    server.mock_room_leave().ok(room_id).mock_once().mount().await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Joined);

    room.leave().await.unwrap();

    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_leave_unknown_room_fails() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    server.mock_room_summary().ok(room_id).mock_once().mount().await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert!(room_preview.state.is_none());

    assert!(client.get_room(room_id).is_none());
}

#[async_test]
async fn test_room_preview_computes_name_if_room_is_known() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    // Given a room with no name but a hero
    let room = assign!(sliding_sync_http::response::Room::new(), {
        name: None,
        heroes: Some(vec![assign!(Hero::new(owned_user_id!("@alice:matrix.org")), {
            name: Some("Alice".to_owned()),
            avatar: None,
        })]),
        joined_count: Some(uint!(1)),
        invited_count: Some(uint!(1)),
    });
    let mut response = sliding_sync_http::Response::new("0".to_owned());
    response.rooms.insert(room_id.to_owned(), room);

    client
        .process_sliding_sync_test_helper(&response, &RequestedRequiredStates::default())
        .await
        .expect("Failed to process sync");

    // When we get its preview
    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();

    // Its name is computed from its heroes
    assert_eq!(room_preview.name.unwrap(), "Alice");
}
