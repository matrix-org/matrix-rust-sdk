use std::time::Duration;

use matrix_sdk::config::SyncSettings;
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{async_test, test_json};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync};

#[async_test]
async fn forget_room() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/forget$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::LEAVE_SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);

    room.forget().await.unwrap();
}

#[async_test]
async fn rejoin_room() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/join"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "room_id": *test_json::DEFAULT_SYNC_ROOM_ID })),
        )
        .mount(&server)
        .await;
    mock_sync(&server, &*test_json::LEAVE_SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);

    room.join().await.unwrap();
    assert!(!room.is_state_fully_synced())
}
