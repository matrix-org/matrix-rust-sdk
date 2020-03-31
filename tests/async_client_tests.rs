use matrix_sdk::identifiers::UserId;
use matrix_sdk::{AsyncClient, Session, SyncSettings};

use mockito::{mock, Matcher};
use tokio::runtime::Runtime;
use url::Url;

use std::convert::TryFrom;
use std::str::FromStr;
use std::time::Duration;

#[tokio::test]
async fn login() {
    let homeserver = Url::from_str(&mockito::server_url()).unwrap();

    let _m = mock("POST", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body_from_file("tests/data/login_response.json")
        .create();

    let mut client = AsyncClient::new(homeserver, None).unwrap();

    client
        .login("example", "wordpass", None, None)
        .await
        .unwrap();

    let logged_in = client.logged_in().await;
    assert!(logged_in, "Clint should be logged in");
}

#[tokio::test]
async fn sync() {
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

    let response = client.sync(sync_settings).await.unwrap();

    assert_ne!(response.next_batch, "");

    assert!(client.sync_token().await.is_some());
}

#[tokio::test]
async fn room_names() {
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

    let _response = client.sync(sync_settings).await.unwrap();

    assert_eq!(vec!["tutorial"], client.get_room_names().await);
    assert_eq!(
        Some("tutorial".into()),
        client.get_room_name("!SVkFJHzfwvuaIEawgC:localhost").await
    );
}

#[tokio::test]
async fn current_room() {
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

    let _response = client.sync(sync_settings).await.unwrap();

    assert_eq!(
        Some("!SVkFJHzfwvuaIEawgC:localhost".into()),
        client.current_room_id().await.map(|id| id.to_string())
    );
}
