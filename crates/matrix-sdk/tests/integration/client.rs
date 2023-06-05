use std::{collections::BTreeMap, str::FromStr, time::Duration};

use assert_matches::assert_matches;
use futures_util::FutureExt;
use matrix_sdk::{
    config::SyncSettings,
    media::{MediaFormat, MediaRequest, MediaThumbnailSize},
    sync::RoomUpdate,
    RumaApiError, Session,
};
use matrix_sdk_test::{async_test, test_json};
use ruma::{
    api::client::{
        self as client_api,
        account::register::{v3::Request as RegistrationRequest, RegistrationKind},
        directory::{
            get_public_rooms,
            get_public_rooms_filtered::{self, v3::Request as PublicRoomsFilterRequest},
        },
        media::get_content_thumbnail::v3::Method,
        session::get_login_types::v3::LoginType,
        uiaa,
    },
    assign, device_id,
    directory::Filter,
    events::room::{message::ImageMessageEventContent, ImageInfo, MediaSource},
    mxc_uri, room_id, uint, user_id,
};
use serde_json::{from_value as from_json_value, json, to_value as to_json_value};
use url::Url;
use wiremock::{
    matchers::{header, method, path, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync, no_retry_test_client};

#[async_test]
async fn login() {
    let (client, server) = no_retry_test_client().await;
    let homeserver = Url::from_str(&server.uri()).unwrap();

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_TYPES))
        .mount(&server)
        .await;

    let can_password = client
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Password(_)));
    assert!(can_password);

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    client.login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().await, homeserver);
}

#[async_test]
async fn login_with_discovery() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_WITH_DISCOVERY))
        .mount(&server)
        .await;

    client.login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().await.as_str(), "https://example.org/");
}

#[async_test]
async fn login_no_discovery() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    client.login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().await, Url::parse(&server.uri()).unwrap());
}

#[async_test]
#[cfg(feature = "sso-login")]
async fn login_with_sso() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

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
        .await
        .unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
}

#[async_test]
async fn login_with_sso_token() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_TYPES))
        .mount(&server)
        .await;

    let can_sso = client
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Sso(_)));
    assert!(can_sso);

    let sso_url = client.get_sso_login_url("http://127.0.0.1:3030", None).await;
    sso_url.unwrap();

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    client.login_token("averysmalltoken").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
}

#[async_test]
async fn login_error() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(403).set_body_json(&*test_json::LOGIN_RESPONSE_ERR))
        .mount(&server)
        .await;

    if let Err(err) = client.login_username("example", "wordpass").send().await {
        if let Some(RumaApiError::ClientApi(client_api::Error { status_code, body })) =
            err.as_ruma_api_error()
        {
            assert_eq!(*status_code, http::StatusCode::from_u16(403).unwrap());

            if let client_api::error::ErrorBody::Standard { kind, message } = body {
                if *kind != client_api::error::ErrorKind::Forbidden {
                    panic!("found the wrong `ErrorKind` {kind:?}, expected `Forbidden");
                }

                assert_eq!(message, "Invalid password");
            } else {
                panic!("non-standard error body")
            }
        } else {
            panic!("found the wrong `Error` type {err:?}, expected `Error::RumaResponse");
        }
    } else {
        panic!("this request should return an `Err` variant")
    }
}

#[async_test]
async fn register_error() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/register"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(&*test_json::REGISTRATION_RESPONSE_ERR),
        )
        .mount(&server)
        .await;

    let user = assign!(RegistrationRequest::new(), {
        username: Some("user".to_owned()),
        password: Some("password".to_owned()),
        auth: Some(uiaa::AuthData::FallbackAcknowledgement(
            uiaa::FallbackAcknowledgement::new("foobar".to_owned()),
        )),
        kind: RegistrationKind::User,
    });

    if let Err(err) = client.register(user).await {
        if let Some(client_api::Error { status_code, body }) = err.as_client_api_error() {
            assert_eq!(*status_code, http::StatusCode::from_u16(403).unwrap());
            if let client_api::error::ErrorBody::Standard { kind, message } = body {
                if *kind != client_api::error::ErrorKind::Forbidden {
                    panic!("found the wrong `ErrorKind` {kind:?}, expected `Forbidden");
                }

                assert_eq!(message, "Invalid password");
            } else {
                panic!("non-standard error body")
            }
        } else {
            panic!("found the wrong `Error` type {err:#?}, expected `UiaaResponse`");
        }
    } else {
        panic!("this request should return an `Err` variant")
    }
}

#[async_test]
async fn sync() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let response = client.sync_once(sync_settings).await.unwrap();

    assert_ne!(response.next_batch, "");
}

#[async_test]
async fn devices() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/devices"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::DEVICES))
        .mount(&server)
        .await;

    client.devices().await.unwrap();
}

#[async_test]
async fn delete_devices() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/delete_devices"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "flows": [
                {
                    "stages": [
                        "m.login.password"
                    ]
                }
            ],
            "params": {},
            "session": "vBslorikviAjxzYBASOBGfPp"
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/delete_devices"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "flows": [
                {
                    "stages": [
                        "m.login.password"
                    ]
                }
            ],
            "params": {},
            "session": "vBslorikviAjxzYBASOBGfPp"
        })))
        .mount(&server)
        .await;

    let devices = &[device_id!("DEVICEID").to_owned()];

    if let Err(e) = client.delete_devices(devices, None).await {
        if let Some(info) = e.as_uiaa_response() {
            let mut auth_parameters = BTreeMap::new();

            let identifier = json!({
                "type": "m.id.user",
                "user": "example",
            });
            auth_parameters.insert("identifier".to_owned(), identifier);
            auth_parameters.insert("password".to_owned(), "wordpass".into());

            let auth_data = uiaa::AuthData::Password(assign!(
                uiaa::Password::new(
                    uiaa::UserIdentifier::UserIdOrLocalpart("example".to_owned()),
                    "wordpass".to_owned(),
                ), {
                    session: info.session.clone(),
                }
            ));

            client.delete_devices(devices, Some(auth_data)).await.unwrap();
        }
    }
}

#[async_test]
async fn resolve_room_alias() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/directory/room/%23alias:example.org"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::GET_ALIAS))
        .mount(&server)
        .await;

    let alias = ruma::room_alias_id!("#alias:example.org");
    client.resolve_room_alias(alias).await.unwrap();
}

#[async_test]
async fn join_leave_room() {
    let room_id = &test_json::DEFAULT_SYNC_ROOM_ID;
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let room = client.get_joined_room(room_id);
    assert!(room.is_none());

    let sync_token = client.sync_once(SyncSettings::default()).await.unwrap().next_batch;

    let room = client.get_left_room(room_id);
    assert!(room.is_none());

    let room = client.get_joined_room(room_id);
    assert!(room.is_some());

    mock_sync(&server, &*test_json::LEAVE_SYNC_EVENT, Some(sync_token.clone())).await;

    client.sync_once(SyncSettings::default().token(sync_token)).await.unwrap();

    let room = client.get_joined_room(room_id);
    assert!(room.is_none());

    let room = client.get_left_room(room_id);
    assert!(room.is_some());
}

#[async_test]
async fn join_room_by_id() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/join"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_ID))
        .mount(&server)
        .await;

    let room_id = room_id!("!testroom:example.org");

    assert_eq!(
        // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId
        // field
        client.join_room_by_id(room_id).await.unwrap().room_id(),
        room_id
    );
}

#[async_test]
async fn join_room_by_id_or_alias() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/join/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_ID))
        .mount(&server)
        .await;

    let room_id = room_id!("!testroom:example.org").into();

    assert_eq!(
        // this is the `join_by_room_id::Response` but since no PartialEq we check the RoomId
        // field
        client
            .join_room_by_id_or_alias(room_id, &["server.com".try_into().unwrap()])
            .await
            .unwrap()
            .room_id(),
        room_id!("!testroom:example.org")
    );
}

#[async_test]
async fn room_search_all() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/publicRooms"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS))
        .mount(&server)
        .await;

    let get_public_rooms::v3::Response { chunk, .. } =
        client.public_rooms(Some(10), None, None).await.unwrap();
    assert_eq!(chunk.len(), 1);
}

#[async_test]
async fn room_search_filtered() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/publicRooms"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::PUBLIC_ROOMS))
        .mount(&server)
        .await;

    let generic_search_term = Some("cheese".to_owned());
    let filter = assign!(Filter::new(), { generic_search_term });
    let request = assign!(PublicRoomsFilterRequest::new(), { filter });

    let get_public_rooms_filtered::v3::Response { chunk, .. } =
        client.public_rooms_filtered(request).await.unwrap();
    assert_eq!(chunk.len(), 1);
}

#[async_test]
async fn invited_rooms() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::INVITE_SYNC, None).await;

    let _response = client.sync_once(SyncSettings::default()).await.unwrap();

    assert!(client.joined_rooms().is_empty());
    assert!(client.left_rooms().is_empty());
    assert!(!client.invited_rooms().is_empty());

    assert!(client.get_invited_room(room_id!("!696r7674:example.com")).is_some());
}

#[async_test]
async fn left_rooms() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::LEAVE_SYNC, None).await;

    let _response = client.sync_once(SyncSettings::default()).await.unwrap();

    assert!(client.joined_rooms().is_empty());
    assert!(!client.left_rooms().is_empty());
    assert!(client.invited_rooms().is_empty());

    assert!(client.get_left_room(&test_json::DEFAULT_SYNC_ROOM_ID).is_some())
}

#[async_test]
async fn get_media_content() {
    let (client, server) = logged_in_client().await;

    let request = MediaRequest {
        source: MediaSource::Plain(mxc_uri!("mxc://localhost/textfile").to_owned()),
        format: MediaFormat::File,
    };

    Mock::given(method("GET"))
        .and(path("/_matrix/media/r0/download/localhost/textfile"))
        .respond_with(ResponseTemplate::new(200).set_body_string("Some very interesting text."))
        .mount(&server)
        .await;

    client.media().get_media_content(&request, false).await.unwrap();
}

#[async_test]
async fn get_media_file() {
    let (client, server) = logged_in_client().await;

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

    Mock::given(method("GET"))
        .and(path("/_matrix/media/r0/download/example.org/image"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("binaryjpegdata", "image/jpeg"))
        .named("get_file")
        .mount(&server)
        .await;

    client.media().get_file(event_content.clone(), false).await.unwrap();

    Mock::given(method("GET"))
        .and(path("/_matrix/media/r0/thumbnail/example.org/image"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("smallerbinaryjpegdata", "image/jpeg"),
        )
        .expect(1)
        .named("get_thumbnail")
        .mount(&server)
        .await;

    client
        .media()
        .get_thumbnail(
            event_content,
            MediaThumbnailSize { method: Method::Scale, width: uint!(100), height: uint!(100) },
            false,
        )
        .await
        .unwrap();
}

#[async_test]
async fn whoami() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/account/whoami"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
        .mount(&server)
        .await;

    let user_id = user_id!("@joe:example.org");

    assert_eq!(client.whoami().await.unwrap().user_id, user_id);
}

#[test]
fn deserialize_session() {
    // First version, or second version without refresh token.
    let json = json!({
        "access_token": "abcd",
        "user_id": "@user:localhost",
        "device_id": "EFGHIJ",
    });
    let session: Session = from_json_value(json).unwrap();
    assert_eq!(session.access_token, "abcd");
    assert_eq!(session.user_id, "@user:localhost");
    assert_eq!(session.device_id, "EFGHIJ");
    assert_eq!(session.refresh_token, None);

    // Second version with refresh_token.
    let json = json!({
        "access_token": "abcd",
        "refresh_token": "wxyz",
        "user_id": "@user:localhost",
        "device_id": "EFGHIJ",
    });
    let session: Session = from_json_value(json).unwrap();
    assert_eq!(session.access_token, "abcd");
    assert_eq!(session.user_id, "@user:localhost");
    assert_eq!(session.device_id, "EFGHIJ");
    assert_eq!(session.refresh_token.as_deref(), Some("wxyz"));
}

#[test]
fn serialize_session() {
    // Without refresh token.
    let mut session = Session {
        access_token: "abcd".to_owned(),
        refresh_token: None,
        user_id: user_id!("@user:localhost").to_owned(),
        device_id: device_id!("EFGHIJ").to_owned(),
    };
    assert_eq!(
        to_json_value(session.clone()).unwrap(),
        json!({
            "access_token": "abcd",
            "user_id": "@user:localhost",
            "device_id": "EFGHIJ",
        })
    );

    // With refresh_token.
    session.refresh_token = Some("wxyz".to_owned());
    assert_eq!(
        to_json_value(session).unwrap(),
        json!({
            "access_token": "abcd",
            "refresh_token": "wxyz",
            "user_id": "@user:localhost",
            "device_id": "EFGHIJ",
        })
    );
}

#[async_test]
async fn room_update_channel() {
    let (client, server) = logged_in_client().await;

    let mut rx = client.subscribe_to_room_updates(room_id!("!SVkFJHzfwvuaIEawgC:localhost"));

    mock_sync(&server, &*test_json::SYNC, None).await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    client.sync_once(sync_settings).await.unwrap();

    let update = rx.recv().now_or_never().unwrap().unwrap();
    let updates = assert_matches!(update, RoomUpdate::Joined { updates, .. } => updates);

    assert_eq!(updates.account_data.len(), 1);
    assert_eq!(updates.ephemeral.len(), 1);
    assert_eq!(updates.state.len(), 9);

    assert!(updates.timeline.limited);
    assert_eq!(updates.timeline.events.len(), 1);
    assert_eq!(updates.timeline.prev_batch, Some("t392-516_47314_0_7_1_1_1_11444_1".to_owned()));

    assert_eq!(updates.unread_notifications.highlight_count, 0);
    assert_eq!(updates.unread_notifications.notification_count, 11);
}
