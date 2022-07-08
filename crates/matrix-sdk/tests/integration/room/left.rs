use std::time::Duration;

use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{async_test, test_json};
use mockito::{mock, Matcher};
use ruma::room_id;

use crate::logged_in_client;

#[async_test]
async fn forget_room() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/forget".to_owned()))
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::LEAVE_SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_left_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    room.forget().await.unwrap();
}
