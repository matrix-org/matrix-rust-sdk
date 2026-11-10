use futures_util::StreamExt;
use matrix_sdk::{room::ParentSpace, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, TestRoom, User, async_test, event_factory::EventFactory};
use ruma::{RoomVersionId, owned_event_id, serde::Raw};
use serde_json::json;
use strass::assert_let;

#[async_test]
async fn test_no_parent_space() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Sync a room with no space parent event.
    let room = server.sync_default_joined_room(&client).await;

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 0);
}

#[async_test]
async fn test_parent_space_undeserializable() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Sync a room with an invalid space parent event.
    let room: matrix_sdk::Room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_state_event(
                Raw::new(&json!({
                    "content": {
                        "canonical": "true", // invalid, must be a boolean
                        "via": [
                            "example.org",
                            "other.example.org"
                        ]
                    },
                    "event_id": "$143273582443PhrSn",
                    "origin_server_ts": 152037280000000_u64,
                    "sender": User::Alice,
                    "state_key": TestRoom::Other,
                    "type": "m.space.parent",
                    "unsigned": {
                        "age": 598971425
                    }
                }))
                .unwrap()
                .cast_unchecked(),
            ),
        )
        .await;

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 0);
}

#[async_test]
async fn test_parent_space_redacted() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().sender(User::Alice);
    let space_parent = TestRoom::Other;
    let space_parent_event_id = owned_event_id!("$space_parent");

    // Sync a room with a redacted space parent event.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_bulk([
                f.space_parent(space_parent, TestRoom::Default)
                    .event_id(&space_parent_event_id)
                    .into(),
                f.redaction(space_parent_event_id).into(),
            ]),
        )
        .await;

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 0);
}

#[async_test]
async fn test_parent_space_unverifiable() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().sender(User::Alice);
    let space_parent = TestRoom::Other;

    // Sync a room with a space parent event, but the parent space is unknown.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.space_parent(space_parent, TestRoom::Default)),
        )
        .await;

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Unverifiable(space_id) = spaces.first().unwrap());
    assert_eq!(space_id, space_parent);
}

#[async_test]
async fn test_parent_space_illegitimate() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().sender(User::Alice);
    let space_parent = TestRoom::Other;

    // Sync a room with a space parent event.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.space_parent(space_parent, TestRoom::Default)),
        )
        .await;

    // Sync the parent space without space child events.
    server.sync_room(&client, JoinedRoomBuilder::new(space_parent)).await;

    // Because there is no space child event in the parent room, the method will
    // try to fetch the members of the room to check if the sender of the
    // m.space.parent event can send one.
    server
        .mock_get_members()
        .ok(vec![f.member(User::Alice).room(space_parent).into()])
        .expect(1)
        .mount()
        .await;

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Illegitimate(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), space_parent);
}

#[async_test]
async fn test_parent_space_reciprocal() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().sender(User::Alice);
    let space_parent = TestRoom::Other;
    let space_child = TestRoom::Default;

    // Sync a room with a space parent event.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.space_parent(space_parent, space_child)),
        )
        .await;

    // Sync the parent space with a space child event for the child room.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(space_parent)
                .add_timeline_event(f.space_child(space_parent, space_child)),
        )
        .await;

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Reciprocal(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), space_parent);
}

#[async_test]
async fn test_parent_space_redacted_reciprocal() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().sender(User::Alice);
    let space_parent = TestRoom::Other;
    let space_child = TestRoom::Default;
    let space_child_event_id = owned_event_id!("$space_child");

    // Sync a room with a space parent event.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.space_parent(space_parent, space_child)),
        )
        .await;

    // Sync the parent space with a redacted space child event for the child
    // room, invalidating the event.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(space_parent).add_timeline_bulk([
                f.space_child(space_parent, space_child).event_id(&space_child_event_id).into(),
                f.redaction(space_child_event_id).into(),
            ]),
        )
        .await;

    // Because there is no valid space child event in the parent room, the
    // method will try to fetch the members of the room to check if the sender
    // of the m.space.parent event can send one.
    server
        .mock_get_members()
        .ok(vec![f.member(User::Alice).room(space_parent).into()])
        .expect(1)
        .mount()
        .await;

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Illegitimate(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), space_parent);
}

#[async_test]
async fn test_parent_space_powerlevel() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let sender = User::Alice;
    let f = EventFactory::new().sender(sender);
    let space_parent = TestRoom::Other;

    // Sync a room with a space parent event.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.space_parent(space_parent, TestRoom::Default)),
        )
        .await;

    // Sync the parent space with no space child event, but a power levels event
    // with a power level for the sender of the m.space.parent event.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(space_parent).add_timeline_bulk([
                f.create(sender, RoomVersionId::V10).into(),
                // We need at least 50 to send a state event by default, we are good here.
                f.power_levels().set_users([(sender, 100)]).into(),
            ]),
        )
        .await;

    // Because there is no space child event in the parent room, the method will
    // try to fetch the members of the room to check if the sender of the
    // m.space.parent event can send one.
    server
        .mock_get_members()
        .ok(vec![f.member(User::Alice).room(space_parent).into()])
        .expect(1)
        .mount()
        .await;

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::WithPowerlevel(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), space_parent);
}

#[async_test]
async fn test_parent_space_powerlevel_too_low() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let sender = User::Alice;
    let f = EventFactory::new().sender(sender);
    let space_parent = TestRoom::Other;

    // Sync a room with a space parent event.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.space_parent(space_parent, TestRoom::Default)),
        )
        .await;

    // Sync the parent space with no space child event, but a power levels event
    // with a power level for the sender of the m.space.parent event.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(space_parent).add_timeline_bulk([
                f.create(sender, RoomVersionId::V10).into(),
                // We need at least 50 to send a state event by default, this is not enough.
                f.power_levels().set_users([(sender, 20)]).into(),
            ]),
        )
        .await;

    // Because there is no space child event in the parent room, the method will
    // try to fetch the members of the room to check if the sender of the
    // m.space.parent event can send one.
    server
        .mock_get_members()
        .ok(vec![f.member(User::Alice).room(space_parent).into()])
        .expect(1)
        .mount()
        .await;

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Illegitimate(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), space_parent);
}
