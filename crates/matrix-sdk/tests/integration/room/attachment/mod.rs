use std::time::Duration;

use matrix_sdk::{
    attachment::{AttachmentConfig, AttachmentInfo, BaseImageInfo, BaseVideoInfo, Thumbnail},
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{async_test, DEFAULT_TEST_ROOM_ID};
use ruma::{
    event_id,
    events::{room::MediaSource, Mentions},
    mxc_uri, owned_mxc_uri, owned_user_id, uint,
};
use serde_json::json;

#[async_test]
async fn test_room_attachment_send() {
    let mock = MatrixMockServer::new().await;

    let expected_event_id = event_id!("$h29iv0s8:example.com");

    mock.mock_room_send()
        .body_matches_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
            }
        }))
        .ok(expected_event_id)
        .mock_once()
        .mount()
        .await;

    mock.mock_upload()
        .expect_mime_type("image/jpeg")
        .ok(mxc_uri!("mxc://example.com/AQwafuaFswefuhsfAFAgsw"))
        .mock_once()
        .mount()
        .await;

    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;
    mock.mock_room_state_encryption().plain().mount().await;

    let response = room
        .send_attachment(
            "image",
            &mime::IMAGE_JPEG,
            b"Hello world".to_vec(),
            AttachmentConfig::new(),
        )
        .await
        .unwrap();

    assert_eq!(expected_event_id, response.event_id);
}

#[async_test]
async fn test_room_attachment_send_info() {
    let mock = MatrixMockServer::new().await;

    let expected_event_id = event_id!("$h29iv0s8:example.com");
    mock.mock_room_send()
        .body_matches_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
            }
        }))
        .ok(expected_event_id)
        .mock_once()
        .mount()
        .await;

    mock.mock_upload()
        .expect_mime_type("image/jpeg")
        .ok(mxc_uri!("mxc://example.com/AQwafuaFswefuhsfAFAgsw"))
        .mock_once()
        .mount()
        .await;

    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;
    mock.mock_room_state_encryption().plain().mount().await;

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

    assert_eq!(expected_event_id, response.event_id)
}

#[async_test]
async fn test_room_attachment_send_wrong_info() {
    let mock = MatrixMockServer::new().await;

    // Note: this mock is NOT called because the height and width are lost, because
    // we're trying to send the attachment as an image, while we provide a
    // `VideoInfo`.
    //
    // So long for static typing.

    mock.mock_room_send()
        .body_matches_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
            }
        }))
        .ok(event_id!("$unused"))
        .mount()
        .await;

    mock.mock_upload()
        .expect_mime_type("image/jpeg")
        .ok(mxc_uri!("mxc://example.com/yo"))
        .mock_once()
        .mount()
        .await;

    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;
    mock.mock_room_state_encryption().plain().mount().await;

    // Here, using `AttachmentInfo::Video`…
    let config = AttachmentConfig::new()
        .info(AttachmentInfo::Video(BaseVideoInfo {
            height: Some(uint!(600)),
            width: Some(uint!(800)),
            duration: Some(Duration::from_millis(3600)),
            size: None,
            blurhash: None,
        }))
        .caption(Some("image caption".to_owned()));

    // But here, using `image/jpeg`.
    let response =
        room.send_attachment("image.jpg", &mime::IMAGE_JPEG, b"Hello world".to_vec(), config).await;

    // In the real-world, this would lead to the size information getting lost,
    // instead of an error during upload. …Is this test any useful?
    response.unwrap_err();
}

#[async_test]
async fn test_room_attachment_send_info_thumbnail() {
    let mock = MatrixMockServer::new().await;

    let media_mxc = owned_mxc_uri!("mxc://example.com/media");
    let thumbnail_mxc = owned_mxc_uri!("mxc://example.com/thumbnail");

    let expected_event_id = event_id!("$h29iv0s8:example.com");

    mock.mock_room_send()
        .body_matches_partial_json(json!({
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
        }))
        .ok(expected_event_id)
        .mock_once()
        .mount()
        .await;

    // First request to /upload: return the thumbnail MXC.
    mock.mock_upload().expect_mime_type("image/jpeg").ok(&thumbnail_mxc).mock_once().mount().await;

    // Second request: return the media MXC.
    mock.mock_upload().expect_mime_type("image/jpeg").ok(&media_mxc).mock_once().mount().await;

    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;
    mock.mock_room_state_encryption().plain().mount().await;

    // Preconditions: nothing is found in the cache.
    let media_request =
        MediaRequestParameters { source: MediaSource::Plain(media_mxc), format: MediaFormat::File };
    let thumbnail_request = MediaRequestParameters {
        source: MediaSource::Plain(thumbnail_mxc.clone()),
        format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(uint!(480), uint!(360))),
    };

    let _ = client.media().get_media_content(&media_request, true).await.unwrap_err();
    let _ = client.media().get_media_content(&thumbnail_request, true).await.unwrap_err();

    // Send the attachment with a thumbnail.
    let config = AttachmentConfig::with_thumbnail(Thumbnail {
        data: b"Thumbnail".to_vec(),
        content_type: mime::IMAGE_JPEG,
        height: uint!(360),
        width: uint!(480),
        size: uint!(3600),
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
    assert_eq!(response.event_id, expected_event_id);

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
        format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(uint!(42), uint!(1337))),
    };
    let _ = client.media().get_media_content(&thumbnail_request, true).await.unwrap_err();
}

#[async_test]
async fn test_room_attachment_send_mentions() {
    let mock = MatrixMockServer::new().await;

    let expected_event_id = event_id!("$h29iv0s8:example.com");

    mock.mock_room_send()
        .body_matches_partial_json(json!({
            "m.mentions": {
                "user_ids": ["@user:localhost"],
            }
        }))
        .ok(expected_event_id)
        .mock_once()
        .mount()
        .await;

    mock.mock_upload()
        .expect_mime_type("image/jpeg")
        .ok(mxc_uri!("mxc://example.com/AQwafuaFswefuhsfAFAgsw"))
        .mock_once()
        .mount()
        .await;

    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;
    mock.mock_room_state_encryption().plain().mount().await;

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

    assert_eq!(expected_event_id, response.event_id);
}
