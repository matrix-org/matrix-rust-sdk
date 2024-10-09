use std::time::Duration;

use assert_matches2::assert_matches;
use matrix_sdk::config::SyncSettings;
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{
    async_test, test_json, GlobalAccountDataTestEvent, LeftRoomBuilder, SyncResponseBuilder,
    DEFAULT_TEST_ROOM_ID,
};
use ruma::{events::direct::DirectEventContent, user_id, OwnedRoomOrAliasId};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client_with_server, mock_sync};

#[async_test]
async fn test_forget_non_direct_room() {
    let (client, server) = logged_in_client_with_server().await;
    let user_id = client.user_id().unwrap();

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/forget$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .named("forget")
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!("/_matrix/client/r0/user/{user_id}/account_data/m.direct")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .named("set_mdirect")
        .expect(0)
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::LEAVE_SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);

    room.forget().await.unwrap();
}

#[async_test]
async fn test_forget_direct_room() {
    let (client, server) = logged_in_client_with_server().await;
    let user_id = client.user_id().unwrap();
    let invited_user_id = user_id!("@invited:localhost");

    // Initialize the direct room.
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_left_room(LeftRoomBuilder::default());
    sync_builder.add_global_account_data_event(GlobalAccountDataTestEvent::Direct);
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);
    assert!(room.is_direct().await.unwrap());
    assert!(room.direct_targets().contains(invited_user_id));

    let direct_account_data = client
        .account()
        .account_data::<DirectEventContent>()
        .await
        .expect("getting m.direct account data failed")
        .expect("no m.direct account data")
        .deserialize()
        .expect("failed to deserialize m.direct account data");
    assert_matches!(direct_account_data.get(invited_user_id), Some(invited_user_dms));
    assert_eq!(invited_user_dms, &[DEFAULT_TEST_ROOM_ID.to_owned()]);

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/forget$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .named("forget")
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!("/_matrix/client/r0/user/{user_id}/account_data/m.direct")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .named("set_mdirect")
        .expect(1)
        .mount(&server)
        .await;

    room.forget().await.unwrap();
}

#[async_test]
async fn test_rejoin_room() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/join"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "room_id": *DEFAULT_TEST_ROOM_ID })),
        )
        .mount(&server)
        .await;
    mock_sync(&server, &*test_json::LEAVE_SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);

    room.join().await.unwrap();
    assert!(!room.is_state_fully_synced())
}

#[async_test]
async fn test_knocking() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/unstable/xyz.amorgan.knock/knock/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "room_id": *DEFAULT_TEST_ROOM_ID })),
        )
        .mount(&server)
        .await;
    mock_sync(&server, &*test_json::LEAVE_SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);

    let room =
        client.knock(OwnedRoomOrAliasId::from((*DEFAULT_TEST_ROOM_ID).to_owned())).await.unwrap();
    assert_eq!(room.state(), RoomState::Knocked);
}
