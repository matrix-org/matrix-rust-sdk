#[cfg(feature = "experimental-sliding-sync")]
use js_int::uint;
use matrix_sdk::{config::SyncSettings, test_utils::logged_in_client_with_server};
#[cfg(feature = "experimental-sliding-sync")]
use matrix_sdk_base::sliding_sync;
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{
    async_test, InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, SyncResponseBuilder,
};
#[cfg(feature = "experimental-sliding-sync")]
use ruma::{api::client::sync::sync_events::v5::response::Hero, assign};
use ruma::{
    events::room::member::MembershipState, owned_user_id, room_id, space::SpaceRoomJoinRule, RoomId,
};
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

    mock_unknown_summary(
        room_id,
        None,
        SpaceRoomJoinRule::Knock,
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
async fn test_room_preview_leave_knocked() {
    let (client, server) = logged_in_client_with_server().await;
    let room_id = room_id!("!room:localhost");

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_knocked_room(KnockedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    mock_unknown_summary(
        room_id,
        None,
        SpaceRoomJoinRule::Knock,
        Some(MembershipState::Knock),
        &server,
    )
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

    mock_unknown_summary(room_id, None, SpaceRoomJoinRule::Knock, None, &server).await;

    let room_preview = client.get_room_preview(room_id.into(), Vec::new()).await.unwrap();
    assert!(room_preview.state.is_none());

    assert!(client.get_room(room_id).is_none());
}

#[cfg(feature = "experimental-sliding-sync")]
#[async_test]
async fn test_room_preview_computes_name_if_room_is_known() {
    let (client, _) = logged_in_client_with_server().await;
    let room_id = room_id!("!room:localhost");

    // Given a room with no name but a hero
    let room = assign!(sliding_sync::http::response::Room::new(), {
        name: None,
        heroes: Some(vec![assign!(Hero::new(owned_user_id!("@alice:matrix.org")), {
            name: Some("Alice".to_owned()),
            avatar: None,
        })]),
        joined_count: Some(uint!(1)),
        invited_count: Some(uint!(1)),
    });
    let mut response = sliding_sync::http::Response::new("0".to_owned());
    response.rooms.insert(room_id.to_owned(), room);

    client.process_sliding_sync_test_helper(&response).await.expect("Failed to process sync");

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
    join_rule: SpaceRoomJoinRule,
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
