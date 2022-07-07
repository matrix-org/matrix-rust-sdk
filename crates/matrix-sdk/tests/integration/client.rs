// mockito (the http mocking library) is not supported for wasm32
#![cfg(not(target_arch = "wasm32"))]

use std::{collections::BTreeMap, str::FromStr, time::Duration};

use matrix_sdk::{
    config::SyncSettings,
    media::{MediaFormat, MediaRequest, MediaThumbnailSize},
    Error, HttpError, RumaApiError,
};
use matrix_sdk_test::{async_test, test_json};
use mockito::{mock, Matcher};
use ruma::{
    api::{
        client::{
            self as client_api,
            account::register::{v3::Request as RegistrationRequest, RegistrationKind},
            directory::{
                get_public_rooms,
                get_public_rooms_filtered::{self, v3::Request as PublicRoomsFilterRequest},
            },
            media::get_content_thumbnail::v3::Method,
            session::get_login_types::v3::LoginType,
            uiaa::{self, UiaaResponse},
        },
        error::{FromHttpResponseError, ServerError},
    },
    assign, device_id,
    directory::Filter,
    events::room::{message::ImageMessageEventContent, ImageInfo, MediaSource},
    mxc_uri, room_id, uint, user_id,
};
use serde_json::json;
use url::Url;

use crate::{logged_in_client, no_retry_test_client};

#[async_test]
async fn set_homeserver() {
    let client = no_retry_test_client().await;
    let homeserver = Url::from_str("http://example.com/").unwrap();
    client.set_homeserver(homeserver.clone()).await;

    assert_eq!(client.homeserver().await, homeserver);
}

#[async_test]
async fn login() {
    let homeserver = Url::from_str(&mockito::server_url()).unwrap();
    let client = no_retry_test_client().await;

    let _m_types = mock("GET", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body(test_json::LOGIN_TYPES.to_string())
        .create();

    let can_password = client
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Password(_)));
    assert!(can_password);

    let _m_login = mock("POST", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body(test_json::LOGIN.to_string())
        .create();

    client.login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().await, homeserver);
}

#[async_test]
async fn login_with_discovery() {
    let client = no_retry_test_client().await;

    let _m_login = mock("POST", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body(test_json::LOGIN_WITH_DISCOVERY.to_string())
        .create();

    client.login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().await.as_str(), "https://example.org/");
}

#[async_test]
async fn login_no_discovery() {
    let client = no_retry_test_client().await;

    let _m_login = mock("POST", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body(test_json::LOGIN.to_string())
        .create();

    client.login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().await, Url::parse(&mockito::server_url()).unwrap());
}

#[async_test]
#[cfg(feature = "sso-login")]
async fn login_with_sso() {
    let _m_login = mock("POST", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body(test_json::LOGIN.to_string())
        .create();

    let _homeserver = Url::from_str(&mockito::server_url()).unwrap();
    let client = no_retry_test_client().await;
    let idp = ruma::api::client::session::get_login_types::v3::IdentityProvider::new(
        "some-id".to_owned(),
        "idp-name".to_owned(),
    );
    client
        .login_sso(|sso_url| async move {
            let sso_url = Url::parse(&sso_url).unwrap();

            let (_, redirect) =
                sso_url.query_pairs().find(|(key, _)| key == "redirectUrl").unwrap();

            let mut redirect_url = Url::parse(&redirect).unwrap();
            redirect_url.set_query(Some("loginToken=tinytoken"));

            reqwest::get(redirect_url.to_string()).await.unwrap();

            Ok(())
        })
        .identity_provider_id(&idp.id)
        .send()
        .await
        .unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
}

#[async_test]
async fn login_with_sso_token() {
    let client = no_retry_test_client().await;

    let _m = mock("GET", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body(test_json::LOGIN_TYPES.to_string())
        .create();

    let can_sso = client
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Sso(_)));
    assert!(can_sso);

    let sso_url = client.get_sso_login_url("http://127.0.0.1:3030", None).await;
    assert!(sso_url.is_ok());

    let _m = mock("POST", "/_matrix/client/r0/login")
        .with_status(200)
        .with_body(test_json::LOGIN.to_string())
        .create();

    client.login_token("averysmalltoken").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
}

#[async_test]
async fn login_error() {
    let client = no_retry_test_client().await;

    let _m = mock("POST", "/_matrix/client/r0/login")
        .with_status(403)
        .with_body(test_json::LOGIN_RESPONSE_ERR.to_string())
        .create();

    if let Err(err) = client.login_username("example", "wordpass").send().await {
        if let Error::Http(HttpError::Api(FromHttpResponseError::Server(ServerError::Known(
            RumaApiError::ClientApi(client_api::Error { kind, message, status_code }),
        )))) = err
        {
            if let client_api::error::ErrorKind::Forbidden = kind {
            } else {
                panic!("found the wrong `ErrorKind` {:?}, expected `Forbidden", kind);
            }
            assert_eq!(message, "Invalid password".to_owned());
            assert_eq!(status_code, http::StatusCode::from_u16(403).unwrap());
        } else {
            panic!("found the wrong `Error` type {:?}, expected `Error::RumaResponse", err);
        }
    } else {
        panic!("this request should return an `Err` variant")
    }
}

#[async_test]
async fn register_error() {
    let client = no_retry_test_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/register\?.*$".to_owned()))
        .with_status(403)
        .with_body(test_json::REGISTRATION_RESPONSE_ERR.to_string())
        .create();

    let user = assign!(RegistrationRequest::new(), {
        username: Some("user"),
        password: Some("password"),
        auth: Some(uiaa::AuthData::FallbackAcknowledgement(
            uiaa::FallbackAcknowledgement::new("foobar"),
        )),
        kind: RegistrationKind::User,
    });

    if let Err(err) = client.register(user).await {
        if let HttpError::UiaaError(FromHttpResponseError::Server(ServerError::Known(
            UiaaResponse::MatrixError(client_api::Error { kind, message, status_code }),
        ))) = err
        {
            if let client_api::error::ErrorKind::Forbidden = kind {
            } else {
                panic!("found the wrong `ErrorKind` {:?}, expected `Forbidden", kind);
            }
            assert_eq!(message, "Invalid password".to_owned());
            assert_eq!(status_code, http::StatusCode::from_u16(403).unwrap());
        } else {
            panic!("found the wrong `Error` type {:#?}, expected `UiaaResponse`", err);
        }
    } else {
        panic!("this request should return an `Err` variant")
    }
}

#[async_test]
async fn sync() {
    let client = logged_in_client().await;

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let response = client.sync_once(sync_settings).await.unwrap();

    assert_ne!(response.next_batch, "");

    assert!(client.sync_token().await.is_some());
}

#[async_test]
async fn devices() {
    let client = logged_in_client().await;

    let _m = mock("GET", "/_matrix/client/r0/devices")
        .with_status(200)
        .with_body(test_json::DEVICES.to_string())
        .create();

    assert!(client.devices().await.is_ok());
}

#[async_test]
async fn delete_devices() {
    let client = no_retry_test_client().await;

    let _m = mock("POST", "/_matrix/client/r0/delete_devices")
        .with_status(401)
        .with_body(
            json!({
                "flows": [
                    {
                        "stages": [
                            "m.login.password"
                        ]
                    }
                ],
                "params": {},
                "session": "vBslorikviAjxzYBASOBGfPp"
            })
            .to_string(),
        )
        .create();

    let _m = mock("POST", "/_matrix/client/r0/delete_devices")
        .with_status(401)
        // empty response
        // TODO rename that response type.
        .with_body(test_json::LOGOUT.to_string())
        .create();

    let devices = &[device_id!("DEVICEID").to_owned()];

    if let Err(e) = client.delete_devices(devices, None).await {
        if let Some(info) = e.uiaa_response() {
            let mut auth_parameters = BTreeMap::new();

            let identifier = json!({
                "type": "m.id.user",
                "user": "example",
            });
            auth_parameters.insert("identifier".to_owned(), identifier);
            auth_parameters.insert("password".to_owned(), "wordpass".into());

            let auth_data = uiaa::AuthData::Password(assign!(
                uiaa::Password::new(
                    uiaa::UserIdentifier::UserIdOrLocalpart("example"),
                    "wordpass",
                ), {
                    session: info.session.as_deref(),
                }
            ));

            client.delete_devices(devices, Some(auth_data)).await.unwrap();
        }
    }
}

#[async_test]
async fn resolve_room_alias() {
    let client = no_retry_test_client().await;

    let _m = mock("GET", "/_matrix/client/r0/directory/room/%23alias%3Aexample%2Eorg")
        .with_status(200)
        .with_body(test_json::GET_ALIAS.to_string())
        .create();

    let alias = ruma::room_alias_id!("#alias:example.org");
    assert!(client.resolve_room_alias(alias).await.is_ok());
}

#[async_test]
async fn join_leave_room() {
    let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .with_body(test_json::SYNC.to_string())
        .create();

    let client = logged_in_client().await;
    let session = client.session().unwrap().clone();

    let room = client.get_joined_room(room_id);
    assert!(room.is_none());

    client.sync_once(SyncSettings::default()).await.unwrap();

    let room = client.get_left_room(room_id);
    assert!(room.is_none());

    let room = client.get_joined_room(room_id);
    assert!(room.is_some());

    // test store reloads with correct room state from the state store
    let joined_client = no_retry_test_client().await;
    joined_client.restore_login(session).await.unwrap();

    // joined room reloaded from state store
    joined_client.sync_once(SyncSettings::default()).await.unwrap();
    let room = joined_client.get_joined_room(room_id);
    assert!(room.is_some());

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .with_body(test_json::LEAVE_SYNC_EVENT.to_string())
        .create();

    joined_client.sync_once(SyncSettings::default()).await.unwrap();

    let room = joined_client.get_joined_room(room_id);
    assert!(room.is_none());

    let room = joined_client.get_left_room(room_id);
    assert!(room.is_some());
}

#[async_test]
async fn join_room_by_id() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/join".to_owned()))
        .with_status(200)
        .with_body(test_json::ROOM_ID.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let room_id = room_id!("!testroom:example.org");

    assert_eq!(
        // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId
        // field
        client.join_room_by_id(room_id).await.unwrap().room_id,
        room_id
    );
}

#[async_test]
async fn join_room_by_id_or_alias() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/join/".to_owned()))
        .with_status(200)
        .with_body(test_json::ROOM_ID.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let room_id = room_id!("!testroom:example.org").into();

    assert_eq!(
        // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId
        // field
        client
            .join_room_by_id_or_alias(room_id, &["server.com".try_into().unwrap()])
            .await
            .unwrap()
            .room_id,
        room_id!("!testroom:example.org")
    );
}

#[async_test]
async fn room_search_all() {
    let client = no_retry_test_client().await;

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/publicRooms".to_owned()))
        .with_status(200)
        .with_body(test_json::PUBLIC_ROOMS.to_string())
        .create();

    let get_public_rooms::v3::Response { chunk, .. } =
        client.public_rooms(Some(10), None, None).await.unwrap();
    assert_eq!(chunk.len(), 1);
}

#[async_test]
async fn room_search_filtered() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/publicRooms".to_owned()))
        .with_status(200)
        .with_body(test_json::PUBLIC_ROOMS.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let generic_search_term = Some("cheese");
    let filter = assign!(Filter::new(), { generic_search_term });
    let request = assign!(PublicRoomsFilterRequest::new(), { filter });

    let get_public_rooms_filtered::v3::Response { chunk, .. } =
        client.public_rooms_filtered(request).await.unwrap();
    assert_eq!(chunk.len(), 1);
}

#[async_test]
async fn invited_rooms() {
    let client = logged_in_client().await;

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::INVITE_SYNC.to_string())
        .create();

    let _response = client.sync_once(SyncSettings::default()).await.unwrap();

    assert!(client.joined_rooms().is_empty());
    assert!(client.left_rooms().is_empty());
    assert!(!client.invited_rooms().is_empty());

    assert!(client.get_invited_room(room_id!("!696r7674:example.com")).is_some());
}

#[async_test]
async fn left_rooms() {
    let client = logged_in_client().await;

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::LEAVE_SYNC.to_string())
        .create();

    let _response = client.sync_once(SyncSettings::default()).await.unwrap();

    assert!(client.joined_rooms().is_empty());
    assert!(!client.left_rooms().is_empty());
    assert!(client.invited_rooms().is_empty());

    assert!(client.get_left_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).is_some())
}

#[async_test]
async fn get_media_content() {
    let client = logged_in_client().await;

    let request = MediaRequest {
        source: MediaSource::Plain(mxc_uri!("mxc://localhost/textfile").to_owned()),
        format: MediaFormat::File,
    };

    let m = mock(
        "GET",
        Matcher::Regex(r"^/_matrix/media/r0/download/localhost/textfile\?.*$".to_owned()),
    )
    .with_status(200)
    .with_body("Some very interesting text.")
    .expect(2)
    .create();

    assert!(client.get_media_content(&request, true).await.is_ok());
    assert!(client.get_media_content(&request, true).await.is_ok());
    assert!(client.get_media_content(&request, false).await.is_ok());
    m.assert();
}

#[async_test]
async fn get_media_file() {
    let client = logged_in_client().await;

    let event_content = ImageMessageEventContent::plain(
        "filename.jpg".into(),
        mxc_uri!("mxc://example.org/image").to_owned(),
        Some(Box::new(assign!(ImageInfo::new(), {
            height: Some(uint!(398)),
            width: Some(uint!(394)),
            mimetype: Some("image/jpeg".into()),
            size: Some(uint!(31037)),
        }))),
    );

    let m = mock(
        "GET",
        Matcher::Regex(r"^/_matrix/media/r0/download/example%2Eorg/image\?.*$".to_owned()),
    )
    .with_status(200)
    .with_body("binaryjpegdata")
    .create();

    assert!(client.get_file(event_content.clone(), true).await.is_ok());
    assert!(client.get_file(event_content.clone(), true).await.is_ok());
    m.assert();

    let m = mock(
        "GET",
        Matcher::Regex(r"^/_matrix/media/r0/thumbnail/example%2Eorg/image\?.*$".to_owned()),
    )
    .with_status(200)
    .with_body("smallerbinaryjpegdata")
    .create();

    assert!(client
        .get_thumbnail(
            event_content,
            MediaThumbnailSize { method: Method::Scale, width: uint!(100), height: uint!(100) },
            true
        )
        .await
        .is_ok());
    m.assert();
}

#[async_test]
async fn whoami() {
    let client = logged_in_client().await;

    let _m = mock("GET", "/_matrix/client/r0/account/whoami")
        .with_status(200)
        .with_body(test_json::WHOAMI.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let user_id = user_id!("@joe:example.org");

    assert_eq!(client.whoami().await.unwrap().user_id, user_id);
}
