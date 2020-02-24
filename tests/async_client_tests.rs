use matrix_sdk::identifiers::UserId;
use matrix_sdk::{AsyncClient, Session, SyncSettings};

use mockito::{mock, Matcher};
use tokio::runtime::Runtime;
use url::Url;

use std::convert::TryFrom;
use std::str::FromStr;

#[test]
fn login() {
    let mut rt = Runtime::new().unwrap();

    let homeserver = Url::from_str(&mockito::server_url()).unwrap();

    let _m = mock("POST", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body_from_file("tests/data/login_response.json")
        .create();

    let mut client = AsyncClient::new(homeserver, None).unwrap();

    rt.block_on(client.login("example", "wordpass", None))
        .unwrap();

    assert!(client.logged_in(), "Clint should be logged in");
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

    let sync_settings = SyncSettings::new().timeout(3000).unwrap();

    rt.block_on(client.sync(sync_settings)).unwrap();
}
