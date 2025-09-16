use assert_matches::assert_matches;
use js_int::uint;
use matrix_sdk::{
    config::SyncSettings,
    test_utils::{logged_in_client_with_server, mocks::MatrixMockServer},
};
use matrix_sdk_base::{RequestedRequiredStates, RoomState};
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, SyncResponseBuilder, async_test,
};
use ruma::{
    RoomId,
    api::client::sync::sync_events::v5::{self as sliding_sync_http, response::Hero},
    assign,
    events::room::member::MembershipState,
    owned_user_id,
    room::JoinRuleKind,
    room_id,
};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path_regex},
};

use crate::mock_sync;

#[async_test]
async fn test_room_preview_leave_invited() {
    let (client, server) = logged_in_client_with_server().await;
    let room_id = room_id!("!room:localhost");

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_invited_room(InvitedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    mock_unknown_summary(
        room_id,
        None,
        JoinRuleKind::Knock,
        Some(MembershipState::Invite),
        &server,
    )
    .await;
    mock_leave(room_id, &server).await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Invited);

    client.get_room(room_id).unwrap().leave().await.unwrap();

    assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_invite_leave_room_summary_msc3266_disabled() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!room:localhost");

    server.sync_room(&client, InvitedRoomBuilder::new(room_id)).await;

    // A preview should be built from the sync data above
    let preview = client
        .get_room_preview(room_id.into(), Vec::new())
        .await
        .expect("Room preview should be retrieved");

    assert_eq!(preview.room_id, room_id);
    assert_matches!(preview.state.unwrap(), RoomState::Invited);

    server.mock_room_leave().ok(room_id).expect(1).mount().await;

    client.get_room(room_id).unwrap().leave().await.unwrap();

    assert_matches!(client.get_room(room_id).unwrap().state(), RoomState::Left);
    assert_matches!(
        client.get_room_preview(room_id.into(), Vec::new()).await.unwrap().state.unwrap(),
        RoomState::Left
    );
}

#[async_test]
async fn test_room_preview_leave_knocked() {
    let (client, server) = logged_in_client_with_server().await;
    let room_id = room_id!("!room:localhost");

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_knocked_room(KnockedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    mock_unknown_summary(room_id, None, JoinRuleKind::Knock, Some(MembershipState::Knock), &server)
        .await;
    mock_leave(room_id, &server).await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Knocked);

    let room = client.get_room(room_id).unwrap();
    room.leave().await.unwrap();

    assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_leave_joined() {
    let (client, server) = logged_in_client_with_server().await;
    let room_id = room_id!("!room:localhost");

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    mock_leave(room_id, &server).await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Joined);

    let room = client.get_room(room_id).unwrap();
    room.leave().await.unwrap();

    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_leave_unknown_room_fails() {
    let (client, server) = logged_in_client_with_server().await;
    let room_id = room_id!("!room:localhost");

    mock_unknown_summary(room_id, None, JoinRuleKind::Knock, None, &server).await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert!(room_preview.state.is_none());

    assert!(client.get_room(room_id).is_none());
}

#[async_test]
async fn test_room_preview_computes_name_if_room_is_known() {
    let (client, _) = logged_in_client_with_server().await;
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

async fn mock_leave(room_id: &RoomId, server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/leave"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "room_id": room_id,
        })))
        .mount(server)
        .await
}

async fn mock_unknown_summary(
    room_id: &RoomId,
    alias: Option<String>,
    join_rule: JoinRuleKind,
    membership: Option<MembershipState>,
    server: &MockServer,
) {
    let body = if let Some(alias) = alias {
        json!({
            "room_id": room_id,
            "canonical_alias": alias,
            "guest_can_join": true,
            "num_joined_members": 1,
            "world_readable": true,
            "join_rule": join_rule,
            "membership": membership,
        })
    } else {
        json!({
            "room_id": room_id,
            "guest_can_join": true,
            "num_joined_members": 1,
            "world_readable": true,
            "join_rule": join_rule,
            "membership": membership,
        })
    };
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/unstable/im.nheko.summary/rooms/.*/summary"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await
}
