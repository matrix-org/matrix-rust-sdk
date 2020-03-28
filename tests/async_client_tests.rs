use matrix_sdk::identifiers::UserId;
use matrix_sdk::{AsyncClient, Session, SyncSettings};

use mockito::{mock, Matcher};
use tokio::runtime::Runtime;
use url::Url;

use std::convert::TryFrom;
use std::str::FromStr;
use std::time::Duration;

#[test]
fn login() {
    let mut rt = Runtime::new().unwrap();

    let homeserver = Url::from_str(&mockito::server_url()).unwrap();

    let _m = mock("POST", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body_from_file("tests/data/login_response.json")
        .create();

    let mut client = AsyncClient::new(homeserver, None).unwrap();

    rt.block_on(client.login("example", "wordpass", None, None))
        .unwrap();

    let logged_in = rt.block_on(client.logged_in());
    assert!(logged_in, "Clint should be logged in");
}

#[test]
fn sync() {
    let mut rt = Runtime::new().unwrap();

    let homeserver = Url::from_str(&mockito::server_url()).unwrap();

    let session = Session {
        access_token: "1234".to_owned(),
        user_id: UserId::try_from("@example:example.com").unwrap(),
        device_id: "DEVICEID".to_owned(),
    };

    let _m = mock(
        "GET",
        Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
    )
    .with_status(200)
    .with_body_from_file("tests/data/sync.json")
    .create();

    let mut client = AsyncClient::new(homeserver, Some(session)).unwrap();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let response = rt.block_on(client.sync(sync_settings)).unwrap();

    assert_ne!(response.next_batch, "");

    assert!(rt.block_on(client.sync_token()).is_some());
}

#[test]
fn timeline() {
    let mut rt = Runtime::new().unwrap();

    let homeserver = Url::from_str(&mockito::server_url()).unwrap();

    let session = Session {
        access_token: "1234".to_owned(),
        user_id: UserId::try_from("@example:example.com").unwrap(),
        device_id: "DEVICEID".to_owned(),
    };

    let _m = mock(
        "GET",
        Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_string()),
    )
    .with_status(200)
    .with_body_from_file("tests/data/sync.json")
    .create();

    let mut client = AsyncClient::new(homeserver, Some(session)).unwrap();

    let sync_settings = SyncSettings::new().timeout(3000).unwrap();

    let _response = rt.block_on(client.sync(sync_settings)).unwrap();

    assert_eq!(vec!["tutorial"], rt.block_on(client.get_room_names()));
    assert_eq!(
        Some("tutorial".into()),
        rt.block_on(client.get_room_name("!SVkFJHzfwvuaIEawgC:localhost"))
    );

    // rt.block_on(async { println!("{:#?}", &client.base_client().read().await.joined_rooms ) });
}
