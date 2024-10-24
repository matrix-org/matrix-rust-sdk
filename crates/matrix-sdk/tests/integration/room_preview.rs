use assert_matches2::assert_matches;
use matrix_sdk::{config::SyncSettings, test_utils::logged_in_client_with_server, Error};
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{
    async_test, InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, SyncResponseBuilder,
};
use ruma::{room_id, space::SpaceRoomJoinRule, RoomId};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, MockServer, ResponseTemplate,
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

    mock_leave(room_id, &server).await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Invited);

    room_preview.leave().await.unwrap();

    assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);
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

    mock_leave(room_id, &server).await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert_eq!(room_preview.state.unwrap(), RoomState::Knocked);

    room_preview.leave().await.unwrap();

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

    room_preview.leave().await.unwrap();

    assert_eq!(client.get_room(room_id).unwrap().state(), RoomState::Left);
}

#[async_test]
async fn test_room_preview_leave_unknown_room_fails() {
    let (client, server) = logged_in_client_with_server().await;
    let room_id = room_id!("!room:localhost");

    mock_unknown_summary(room_id, None, SpaceRoomJoinRule::Knock, &server).await;
    mock_leave(room_id, &server).await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert!(room_preview.state.is_none());

    let error = room_preview.leave().await.err();

    assert_matches!(error, Some(Error::WrongRoomPreviewState(_)));
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
    join_rule: SpaceRoomJoinRule,
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
        })
    } else {
        json!({
            "room_id": room_id,
            "guest_can_join": true,
            "num_joined_members": 1,
            "world_readable": true,
            "join_rule": join_rule,
        })
    };
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/unstable/im.nheko.summary/rooms/.*/summary"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await
}
