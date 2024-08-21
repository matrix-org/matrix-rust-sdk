use std::time::Duration;

#[cfg(feature = "image-proc")]
use matrix_sdk::attachment::{ImageFormat, ThumbnailFormat};
use matrix_sdk::{
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseImageInfo, BaseThumbnailInfo, BaseVideoInfo,
        Thumbnail,
    },
    config::SyncSettings,
    test_utils::logged_in_client_with_server,
};
use matrix_sdk_test::{async_test, mocks::mock_encryption_state, test_json, DEFAULT_TEST_ROOM_ID};
use ruma::{event_id, events::Mentions, owned_user_id, uint};
use serde_json::json;
use wiremock::{
    matchers::{body_partial_json, header, method, path, path_regex},
    Mock, ResponseTemplate,
};

use crate::mock_sync;

#[async_test]
async fn test_room_attachment_send() {
    let (client, server) = logged_in_client_with_server().await;

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
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response = room
        .send_attachment(
            "image",
            &mime::IMAGE_JPEG,
            b"Hello world".to_vec(),
            AttachmentConfig::new(),
        )
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn test_room_attachment_send_info() {
    let (client, server) = logged_in_client_with_server().await;

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
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config = AttachmentConfig::new()
        .info(AttachmentInfo::Image(BaseImageInfo {
            height: Some(uint!(600)),
            width: Some(uint!(800)),
            size: None,
            blurhash: None,
        }))
        .caption(Some("image caption".to_owned()));

    let response = room
        .send_attachment("image.jpg", &mime::IMAGE_JPEG, b"Hello world".to_vec(), config)
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn test_room_attachment_send_wrong_info() {
    let (client, server) = logged_in_client_with_server().await;

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
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config = AttachmentConfig::new()
        .info(AttachmentInfo::Video(BaseVideoInfo {
            height: Some(uint!(600)),
            width: Some(uint!(800)),
            duration: Some(Duration::from_millis(3600)),
            size: None,
            blurhash: None,
        }))
        .caption(Some("image caption".to_owned()));

    let response =
        room.send_attachment("image.jpg", &mime::IMAGE_JPEG, b"Hello world".to_vec(), config).await;

    response.unwrap_err();
}

#[async_test]
async fn test_room_attachment_send_info_thumbnail() {
    let (client, server) = logged_in_client_with_server().await;

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
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config = AttachmentConfig::with_thumbnail(Thumbnail {
        data: b"Thumbnail".to_vec(),
        content_type: mime::IMAGE_JPEG,
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

    let response = room
        .send_attachment("image", &mime::IMAGE_JPEG, b"Hello world".to_vec(), config)
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn test_room_attachment_send_mentions() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "m.mentions": {
                "user_ids": ["@user:localhost"],
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
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response = room
        .send_attachment(
            "image",
            &mime::IMAGE_JPEG,
            b"Hello world".to_vec(),
            AttachmentConfig::new()
                .mentions(Some(Mentions::with_user_ids([owned_user_id!("@user:localhost")]))),
        )
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[cfg(feature = "image-proc")]
const IMAGE_BYTES: &[u8] = include_bytes!("matrix-rusty.jpg");

#[cfg(feature = "image-proc")]
#[async_test]
async fn test_room_attachment_generate_thumbnail_original_format() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "url": "mxc://localhost/AQwafuaFswefuhsfAFAgsw",
            "info": {
                "mimetype": "image/jpeg",
                "thumbnail_info": {
                    "h": 600,
                    "w": 600,
                    "mimetype":"image/jpeg",
                },
                "thumbnail_url": "mxc://localhost/AQwafuaFswefuhsfAFAgsw",
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
          "content_uri": "mxc://localhost/AQwafuaFswefuhsfAFAgsw"
        })))
        .expect(2)
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config = AttachmentConfig::new().generate_thumbnail(None, ThumbnailFormat::Original);

    let response = room
        .send_attachment("image", &mime::IMAGE_JPEG, IMAGE_BYTES.to_vec(), config)
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[cfg(feature = "image-proc")]
#[async_test]
async fn test_room_attachment_generate_thumbnail_always_format() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "url": "mxc://localhost/original",
            "info": {
                "mimetype": "image/jpeg",
                "thumbnail_info": {
                    "h": 600,
                    "w": 600,
                    "mimetype":"image/png",
                },
                "thumbnail_url": "mxc://localhost/thumbnail",
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
          "content_uri": "mxc://localhost/original"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/png"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://localhost/thumbnail"
        })))
        .expect(1)
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config =
        AttachmentConfig::new().generate_thumbnail(None, ThumbnailFormat::Always(ImageFormat::Png));

    let response = room
        .send_attachment("image", &mime::IMAGE_JPEG, IMAGE_BYTES.to_vec(), config)
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[cfg(feature = "image-proc")]
#[async_test]
async fn test_room_attachment_generate_thumbnail_not_fallback_format() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "url": "mxc://localhost/AQwafuaFswefuhsfAFAgsw",
            "info": {
                "mimetype": "image/jpeg",
                "thumbnail_info": {
                    "h": 600,
                    "w": 600,
                    "mimetype":"image/jpeg",
                },
                "thumbnail_url": "mxc://localhost/AQwafuaFswefuhsfAFAgsw",
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
          "content_uri": "mxc://localhost/AQwafuaFswefuhsfAFAgsw"
        })))
        .expect(2)
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config = AttachmentConfig::new()
        .generate_thumbnail(None, ThumbnailFormat::Fallback(ImageFormat::Png));

    let response = room
        .send_attachment("image", &mime::IMAGE_JPEG, IMAGE_BYTES.to_vec(), config)
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[cfg(feature = "image-proc")]
#[async_test]
async fn test_room_attachment_generate_thumbnail_bigger_than_image() {
    use matrix_sdk_test::mocks::mock_encryption_state;

    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "url": "mxc://localhost/original",
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
          "content_uri": "mxc://localhost/original"
        })))
        .expect(1)
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config =
        AttachmentConfig::new().generate_thumbnail(Some((1400, 1400)), ThumbnailFormat::Original);

    let response = room
        .send_attachment("image", &mime::IMAGE_JPEG, IMAGE_BYTES.to_vec(), config)
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}
