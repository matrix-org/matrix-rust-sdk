use std::{collections::BTreeMap, time::Duration};

use assert_matches::assert_matches;
use futures_util::FutureExt;
use matrix_sdk::{
    config::SyncSettings,
    media::{MediaFormat, MediaRequest, MediaThumbnailSize},
    sync::RoomUpdate,
};
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{async_test, test_json, DEFAULT_TEST_ROOM_ID};
use ruma::{
    api::client::{
        directory::{
            get_public_rooms,
            get_public_rooms_filtered::{self, v3::Request as PublicRoomsFilterRequest},
        },
        media::get_content_thumbnail::v3::Method,
        uiaa,
    },
    assign, device_id,
    directory::Filter,
    events::{
        direct::DirectEventContent,
        room::{message::ImageMessageEventContent, ImageInfo, MediaSource},
    },
    mxc_uri, room_id, uint, user_id,
};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync, no_retry_test_client};

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
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID);
    assert!(room.is_none());

    let sync_token = client.sync_once(SyncSettings::default()).await.unwrap().next_batch;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Joined);

    mock_sync(&server, &*test_json::LEAVE_SYNC_EVENT, Some(sync_token.clone())).await;

    client.sync_once(SyncSettings::default().token(sync_token)).await.unwrap();

    assert_eq!(room.state(), RoomState::Left);
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);
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

    let room = client.get_room(room_id!("!696r7674:example.com")).unwrap();
    assert_eq!(room.state(), RoomState::Invited);
}

#[async_test]
async fn left_rooms() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::LEAVE_SYNC, None).await;

    let _response = client.sync_once(SyncSettings::default()).await.unwrap();

    assert!(client.joined_rooms().is_empty());
    assert!(!client.left_rooms().is_empty());
    assert!(client.invited_rooms().is_empty());

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Left);
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
    )
    .info(Box::new(assign!(ImageInfo::new(), {
        height: Some(uint!(398)),
        width: Some(uint!(394)),
        mimetype: Some("image/jpeg".into()),
        size: Some(uint!(31037)),
    })));

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

#[async_test]
async fn room_update_channel() {
    let (client, server) = logged_in_client().await;

    let mut rx = client.subscribe_to_room_updates(&DEFAULT_TEST_ROOM_ID);

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

// Check that the `Room::is_encrypted()` is properly deduplicated, meaning we
// only make a single request to the server, and that multiple calls do return
// the same result.
#[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
#[async_test]
async fn request_encryption_event_before_sending() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    client
        .sync_once(SyncSettings::default())
        .await
        .expect("We should be able to performa an initial sync");

    let room =
        client.get_room(&DEFAULT_TEST_ROOM_ID).expect("We should know about our default room");

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.encryption/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "algorithm": "m.megolm.v1.aes-sha2",
                    "rotation_period_ms": 604800000,
                    "rotation_period_msgs": 100
                }))
                // Introduce a delay so the first `is_encrypted()` doesn't finish before we make
                // the second call.
                .set_delay(Duration::from_millis(50)),
        )
        .mount(&server)
        .await;

    let first_handle = tokio::spawn({
        let room = room.to_owned();
        async move { room.to_owned().is_encrypted().await }
    });

    let second_handle = tokio::spawn(async move { room.is_encrypted().await });

    let first_encrypted =
        first_handle.await.unwrap().expect("We should be able to test if the room is encrypted.");
    let second_encrypted =
        second_handle.await.unwrap().expect("We should be able to test if the room is encrypted.");

    assert!(first_encrypted, "We should have found out that the room is encrypted.");
    assert_eq!(
        first_encrypted, second_encrypted,
        "Both attempts to find out if the room is encrypted should return the same result."
    );
}

// Check that we're fetching account data from the server when marking a room as
// a DM.
#[async_test]
async fn marking_room_as_dm() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    client
        .sync_once(SyncSettings::default())
        .await
        .expect("We should be able to performa an initial sync");

    let account_data = client
        .account()
        .account_data::<DirectEventContent>()
        .await
        .expect("We should be able to fetch the account data event from the store");

    assert!(account_data.is_none(), "We should not have any account data initially");

    let bob = user_id!("@bob:example.com");
    let users = vec![bob.to_owned()];

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/user/@example:localhost/account_data/m.direct"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@bob:example.com": [
                "!abcdefgh:example.com",
                "!hgfedcba:example.com"
            ],
            "@alice:example.com": [
                "!abcdefgh:example.com",
            ]
        })))
        .expect(1..)
        .named("m.direct account data GET")
        .mount(&server)
        .await;

    let put_direct_content_matcher = |request: &wiremock::Request| {
        let content: DirectEventContent = request.body_json().expect(
            "The body of the PUT /account_data request should be a valid DirectEventContent",
        );

        let bob_entry = content.get(bob).expect("We should have bob in the direct event content");

        assert_eq!(content.len(), 2, "We should have entries for bob and foo");
        assert_eq!(bob_entry.len(), 3, "Bob should have 3 direct rooms");

        content.len() == 2 && bob_entry.len() == 3
    };

    Mock::given(method("PUT"))
        .and(path("_matrix/client/r0/user/@example:localhost/account_data/m.direct"))
        .and(header("authorization", "Bearer 1234"))
        .and(put_direct_content_matcher)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1..)
        .named("m.direct account data PUT")
        .mount(&server)
        .await;

    client
        .account()
        .mark_as_dm(&DEFAULT_TEST_ROOM_ID, &users)
        .await
        .expect("We should be able to mark the room as a DM");

    server.verify().await;
}
