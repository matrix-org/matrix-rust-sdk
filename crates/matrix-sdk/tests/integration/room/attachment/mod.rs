use std::{sync::Mutex, time::Duration};

use matrix_sdk::{
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseImageInfo, BaseThumbnailInfo, BaseVideoInfo,
        Thumbnail,
    },
    config::SyncSettings,
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    test_utils::logged_in_client_with_server,
};
use matrix_sdk_test::{async_test, mocks::mock_encryption_state, test_json, DEFAULT_TEST_ROOM_ID};
use ruma::{
    event_id,
    events::{room::MediaSource, Mentions},
    owned_mxc_uri, owned_user_id, uint,
};
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

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id);
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

    let media_mxc = owned_mxc_uri!("mxc://example.com/media");
    let thumbnail_mxc = owned_mxc_uri!("mxc://example.com/thumbnail");

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
                "thumbnail_url": thumbnail_mxc,
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    let counter = Mutex::new(0);
    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with({
            // First request: return the thumbnail MXC;
            // Second request: return the media MXC.
            let media_mxc = media_mxc.clone();
            let thumbnail_mxc = thumbnail_mxc.clone();
            move |_: &wiremock::Request| {
                let mut counter = counter.lock().unwrap();
                if *counter == 0 {
                    *counter += 1;
                    ResponseTemplate::new(200).set_body_json(json!({
                      "content_uri": &thumbnail_mxc
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                      "content_uri": &media_mxc
                    }))
                }
            }
        })
        .expect(2)
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    // Preconditions: nothing is found in the cache.
    let media_request =
        MediaRequestParameters { source: MediaSource::Plain(media_mxc), format: MediaFormat::File };
    let thumbnail_request = MediaRequestParameters {
        source: MediaSource::Plain(thumbnail_mxc.clone()),
        format: MediaFormat::Thumbnail(MediaThumbnailSettings {
            method: ruma::media::Method::Scale,
            width: uint!(480),
            height: uint!(360),
            animated: false,
        }),
    };
    let _ = client.media().get_media_content(&media_request, true).await.unwrap_err();
    let _ = client.media().get_media_content(&thumbnail_request, true).await.unwrap_err();

    // Send the attachment with a thumbnail.
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
        .store_in_cache()
        .await
        .unwrap();

    // The event was sent.
    assert_eq!(response.event_id, event_id!("$h29iv0s8:example.com"));

    // The media is immediately cached in the cache store, so we don't need to set
    // up another mock endpoint for getting the media.
    let reloaded = client.media().get_media_content(&media_request, true).await.unwrap();
    assert_eq!(reloaded, b"Hello world");

    // The thumbnail is cached with sensible defaults.
    let reloaded = client.media().get_media_content(&thumbnail_request, true).await.unwrap();
    assert_eq!(reloaded, b"Thumbnail");

    // The thumbnail can't be retrieved as a file.
    let _ = client
        .media()
        .get_media_content(
            &MediaRequestParameters {
                source: MediaSource::Plain(thumbnail_mxc.clone()),
                format: MediaFormat::File,
            },
            true,
        )
        .await
        .unwrap_err();

    // But it is not found when requesting it as a thumbnail with a different size.
    let thumbnail_request = MediaRequestParameters {
        source: MediaSource::Plain(thumbnail_mxc),
        format: MediaFormat::Thumbnail(MediaThumbnailSettings {
            method: ruma::media::Method::Scale,
            width: uint!(42),
            height: uint!(1337),
            animated: false,
        }),
    };
    let _ = client.media().get_media_content(&thumbnail_request, true).await.unwrap_err();
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
