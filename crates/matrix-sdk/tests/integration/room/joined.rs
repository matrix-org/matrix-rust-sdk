use std::{io::Cursor, time::Duration};

use matrix_sdk::{
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseImageInfo, BaseThumbnailInfo, BaseVideoInfo,
        Thumbnail,
    },
    config::SyncSettings,
};
use matrix_sdk_test::{async_test, test_json};
use mockito::{mock, Matcher};
use ruma::{
    api::client::membership::Invite3pidInit, assign, event_id,
    events::room::message::RoomMessageEventContent, mxc_uri, room_id, thirdparty, uint, user_id,
    TransactionId,
};
use serde_json::json;

use crate::logged_in_client;

#[async_test]
async fn invite_user_by_id() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/invite".to_owned()))
        .with_status(200)
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    room.invite_user_by_id(user).await.unwrap();
}

#[async_test]
async fn invite_user_by_3pid() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/invite".to_owned()))
        .with_status(200)
        // empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

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
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/leave".to_owned()))
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    room.leave().await.unwrap();
}

#[async_test]
async fn ban_user() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/ban".to_owned()))
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    room.ban_user(user, None).await.unwrap();
}

#[async_test]
async fn kick_user() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/kick".to_owned()))
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    room.kick_user(user, None).await.unwrap();
}

#[async_test]
async fn read_receipt() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/receipt".to_owned()))
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let event_id = event_id!("$xxxxxx:example.org");
    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    room.read_receipt(event_id).await.unwrap();
}

#[async_test]
async fn read_marker() {
    let client = logged_in_client().await;

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/read_markers".to_owned()))
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let event_id = event_id!("$xxxxxx:example.org");
    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    room.read_marker(event_id, None).await.unwrap();
}

#[async_test]
async fn typing_notice() {
    let client = logged_in_client().await;

    let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/typing".to_owned()))
        .with_status(200)
        // this is an empty JSON object
        .with_body(test_json::LOGOUT.to_string())
        .match_header("authorization", "Bearer 1234")
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    room.typing_notice(true).await.unwrap();
}

#[async_test]
async fn room_state_event_send() {
    use ruma::events::room::member::{MembershipState, RoomMemberEventContent};

    let client = logged_in_client().await;

    let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/state/.*".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::EVENT_ID.to_string())
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");

    let room = client.get_joined_room(room_id).unwrap();

    let avatar_url = mxc_uri!("mxc://example.org/avA7ar");
    let member_event = assign!(RoomMemberEventContent::new(MembershipState::Join), {
        avatar_url: Some(avatar_url.to_owned())
    });
    let response = room.send_state_event(member_event, "").await.unwrap();
    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id);
}

#[async_test]
async fn room_message_send() {
    let client = logged_in_client().await;

    let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/send/".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::EVENT_ID.to_string())
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    let content = RoomMessageEventContent::text_plain("Hello world");
    let txn_id = TransactionId::new();
    let response = room.send(content, Some(&txn_id)).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_attachment_send() {
    let client = logged_in_client().await;

    let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/send/".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .match_body(Matcher::PartialJson(json!({
            "info": {
                "mimetype": "image/jpeg"
            }
        })))
        .with_body(test_json::EVENT_ID.to_string())
        .create();

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/media/r0/upload".to_owned()))
        .with_status(200)
        .match_header("content-type", "image/jpeg")
        .with_body(
            json!({
              "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
            })
            .to_string(),
        )
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    let mut media = Cursor::new("Hello world");

    let response = room
        .send_attachment("image", &mime::IMAGE_JPEG, &mut media, AttachmentConfig::new())
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_attachment_send_info() {
    let client = logged_in_client().await;

    let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/send/".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .match_body(Matcher::PartialJson(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
            }
        })))
        .with_body(test_json::EVENT_ID.to_string())
        .create();

    let upload_mock = mock("POST", Matcher::Regex(r"^/_matrix/media/r0/upload".to_owned()))
        .with_status(200)
        .match_header("content-type", "image/jpeg")
        .with_body(
            json!({
              "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
            })
            .to_string(),
        )
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    let mut media = Cursor::new("Hello world");

    let config = AttachmentConfig::new().info(AttachmentInfo::Image(BaseImageInfo {
        height: Some(uint!(600)),
        width: Some(uint!(800)),
        size: None,
        blurhash: None,
    }));

    let response =
        room.send_attachment("image", &mime::IMAGE_JPEG, &mut media, config).await.unwrap();

    upload_mock.assert();
    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_attachment_send_wrong_info() {
    let client = logged_in_client().await;

    let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/send/".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .match_body(Matcher::PartialJson(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
            }
        })))
        .with_body(test_json::EVENT_ID.to_string())
        .create();

    let _m = mock("POST", Matcher::Regex(r"^/_matrix/media/r0/upload".to_owned()))
        .with_status(200)
        .match_header("content-type", "image/jpeg")
        .with_body(
            json!({
              "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
            })
            .to_string(),
        )
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    let mut media = Cursor::new("Hello world");

    let config = AttachmentConfig::new().info(AttachmentInfo::Video(BaseVideoInfo {
        height: Some(uint!(600)),
        width: Some(uint!(800)),
        duration: Some(Duration::from_millis(3600)),
        size: None,
        blurhash: None,
    }));

    let response = room.send_attachment("image", &mime::IMAGE_JPEG, &mut media, config).await;

    assert!(response.is_err())
}

#[async_test]
async fn room_attachment_send_info_thumbnail() {
    let client = logged_in_client().await;

    let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/send/".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .match_body(Matcher::PartialJson(json!({
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
        .with_body(test_json::EVENT_ID.to_string())
        .create();

    let upload_mock = mock("POST", Matcher::Regex(r"^/_matrix/media/r0/upload".to_owned()))
        .with_status(200)
        .match_header("content-type", "image/jpeg")
        .with_body(
            json!({
              "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
            })
            .to_string(),
        )
        .expect(2)
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

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

    upload_mock.assert();
    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_redact() {
    let client = logged_in_client().await;

    let _m = mock("PUT", Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/redact/.*?/.*?".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::EVENT_ID.to_string())
        .create();

    let _m = mock("GET", Matcher::Regex(r"^/_matrix/client/r0/sync\?.*$".to_owned()))
        .with_status(200)
        .match_header("authorization", "Bearer 1234")
        .with_body(test_json::SYNC.to_string())
        .create();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(room_id!("!SVkFJHzfwvuaIEawgC:localhost")).unwrap();

    let event_id = event_id!("$xxxxxxxx:example.com");

    let txn_id = TransactionId::new();
    let reason = Some("Indecent material");
    let response = room.redact(event_id, reason, Some(txn_id)).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}
