use std::{io::Cursor, time::Duration};

use matrix_sdk::{
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseImageInfo, BaseThumbnailInfo, BaseVideoInfo,
        Thumbnail,
    },
    config::SyncSettings,
};
use matrix_sdk_test::{async_test, test_json};
use ruma::{
    api::client::membership::Invite3pidInit, assign, event_id,
    events::room::message::RoomMessageEventContent, mxc_uri, thirdparty, uint, user_id,
    TransactionId,
};
use serde_json::json;
use wiremock::{
    matchers::{body_partial_json, header, method, path, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync};

#[async_test]
async fn invite_user_by_id() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/invite$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    room.invite_user_by_id(user).await.unwrap();
}

#[async_test]
async fn invite_user_by_3pid() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/invite$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    room.invite_user_by_3pid(
        Invite3pidInit {
            id_server: "example.org",
            id_access_token: "IdToken",
            medium: thirdparty::Medium::Email,
            address: "address",
        }
        .into(),
    )
    .await
    .unwrap();
}

#[async_test]
async fn leave_room() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/leave$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, &*test_json::LEAVE_SYNC_EVENT, Some(sync_token.clone())).await;

    let client_clone = client.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        client_clone.sync_once(SyncSettings::default().token(sync_token)).await
    });

    room.leave().await.unwrap();
}

#[async_test]
async fn ban_user() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/ban$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    room.ban_user(user, None).await.unwrap();
}

#[async_test]
async fn kick_user() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/kick$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    room.kick_user(user, None).await.unwrap();
}

#[async_test]
async fn read_receipt() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let event_id = event_id!("$xxxxxx:example.org");
    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    room.read_receipt(event_id).await.unwrap();
}

#[async_test]
async fn read_marker() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/read_markers$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let event_id = event_id!("$xxxxxx:example.org");
    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    room.read_marker(event_id, None).await.unwrap();
}

#[async_test]
async fn typing_notice() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/typing"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    room.typing_notice(true).await.unwrap();
}

#[async_test]
async fn room_state_event_send() {
    use ruma::events::room::member::{MembershipState, RoomMemberEventContent};

    let (client, server) = logged_in_client().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room_id = &test_json::DEFAULT_SYNC_ROOM_ID;

    let room = client.get_joined_room(room_id).unwrap();

    let avatar_url = mxc_uri!("mxc://example.org/avA7ar");
    let member_event = assign!(RoomMemberEventContent::new(MembershipState::Join), {
        avatar_url: Some(avatar_url.to_owned())
    });
    let response =
        room.send_state_event_for_key(user_id!("@foo:bar.com"), member_event).await.unwrap();
    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id);
}

#[async_test]
async fn room_message_send() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    let content = RoomMessageEventContent::text_plain("Hello world");
    let txn_id = TransactionId::new();
    let response = room.send(content, Some(&txn_id)).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_attachment_send() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    let mut media = Cursor::new("Hello world");

    let response = room
        .send_attachment("image", &mime::IMAGE_JPEG, &mut media, AttachmentConfig::new())
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_attachment_send_info() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    let mut media = Cursor::new("Hello world");

    let config = AttachmentConfig::new().info(AttachmentInfo::Image(BaseImageInfo {
        height: Some(uint!(600)),
        width: Some(uint!(800)),
        size: None,
        blurhash: None,
    }));

    let response =
        room.send_attachment("image", &mime::IMAGE_JPEG, &mut media, config).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_attachment_send_wrong_info() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    let mut media = Cursor::new("Hello world");

    let config = AttachmentConfig::new().info(AttachmentInfo::Video(BaseVideoInfo {
        height: Some(uint!(600)),
        width: Some(uint!(800)),
        duration: Some(Duration::from_millis(3600)),
        size: None,
        blurhash: None,
    }));

    let response = room.send_attachment("image", &mime::IMAGE_JPEG, &mut media, config).await;

    response.unwrap_err();
}

#[async_test]
async fn room_attachment_send_info_thumbnail() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
                "thumbnail_info": {
                    "h": 360,
                    "w": 480,
                    "mimetype":"image/jpeg",
                    "size": 3600,
                },
                "thumbnail_url": "mxc://example.com/AQwafuaFswefuhsfAFAgsw",
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
        .expect(2)
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    let mut media = Cursor::new("Hello world");

    let mut thumbnail_reader = Cursor::new("Thumbnail");

    let config = AttachmentConfig::with_thumbnail(Thumbnail {
        reader: &mut thumbnail_reader,
        content_type: &mime::IMAGE_JPEG,
        info: Some(BaseThumbnailInfo {
            height: Some(uint!(360)),
            width: Some(uint!(480)),
            size: Some(uint!(3600)),
        }),
    })
    .info(AttachmentInfo::Image(BaseImageInfo {
        height: Some(uint!(600)),
        width: Some(uint!(800)),
        size: None,
        blurhash: None,
    }));

    let response =
        room.send_attachment("image", &mime::IMAGE_JPEG, &mut media, config).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_redact() {
    let (client, server) = logged_in_client().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/redact/.*?/.*?"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    let event_id = event_id!("$xxxxxxxx:example.com");

    let txn_id = TransactionId::new();
    let reason = Some("Indecent material");
    let response = room.redact(event_id, reason, Some(txn_id)).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}
