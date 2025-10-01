use std::time::Duration;

use matrix_sdk::{
    attachment::{AttachmentConfig, AttachmentInfo, BaseImageInfo, BaseVideoInfo, Thumbnail},
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    room::reply::{EnforceThread, Reply},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{ALICE, DEFAULT_TEST_ROOM_ID, async_test, event_factory::EventFactory};
use ruma::{
    event_id,
    events::{
        Mentions,
        room::{
            MediaSource,
            message::{ReplyWithinThread, TextMessageEventContent},
        },
    },
    mxc_uri, owned_mxc_uri, owned_user_id, uint,
};
use serde_json::json;

#[async_test]
async fn test_room_attachment_send() {
    let mock = MatrixMockServer::new().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;

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

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_room_attachment_send_in_encrypted_room_has_binary_mime_type() {
    let mock = MatrixMockServer::new().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;

    let expected_event_id = event_id!("$h29iv0s8:example.com");

    mock.mock_room_send().ok(expected_event_id).mock_once().mount().await;

    mock.mock_upload()
        .expect_mime_type("application/octet-stream")
        .ok(mxc_uri!("mxc://example.com/AQwafuaFswefuhsfAFAgsw"))
        .up_to_n_times(2)
        .mount()
        .await;

    // Needed for the message to be sent in an encrypted room
    mock.mock_get_members().ok(Vec::new()).mock_once().mount().await;

    let client = mock.client_builder().build().await;
    let room = mock.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;
    mock.mock_room_state_encryption().encrypted().mount().await;

    let response = room
        .send_attachment(
            "image",
            &mime::IMAGE_JPEG,
            b"Hello world".to_vec(),
            AttachmentConfig::new(),
        )
        .await
        .expect("Failed to send attachment");

    assert_eq!(expected_event_id, response.event_id);
}

#[async_test]
async fn test_room_attachment_send_info() {
    let mock = MatrixMockServer::new().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;

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
            ..Default::default()
        }))
        .caption(Some(TextMessageEventContent::plain("image caption")));

    let response = room
        .send_attachment("image.jpg", &mime::IMAGE_JPEG, b"Hello world".to_vec(), config)
        .await
        .unwrap();

    assert_eq!(expected_event_id, response.event_id)
}

#[async_test]
async fn test_room_attachment_send_wrong_info() {
    let mock = MatrixMockServer::new().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;

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
            ..Default::default()
        }))
        .caption(Some(TextMessageEventContent::plain("image caption")));

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

    mock.mock_authenticated_media_config().ok_default().mount().await;

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
    let config = AttachmentConfig::new()
        .thumbnail(Some(Thumbnail {
            data: b"Thumbnail".to_vec(),
            content_type: mime::IMAGE_JPEG,
            height: uint!(360),
            width: uint!(480),
            size: uint!(3600),
        }))
        .info(AttachmentInfo::Image(BaseImageInfo {
            height: Some(uint!(600)),
            width: Some(uint!(800)),
            ..Default::default()
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

    mock.mock_authenticated_media_config().ok_default().mount().await;

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

#[async_test]
async fn test_room_attachment_reply_outside_thread() {
    let mock = MatrixMockServer::new().await;

    let expected_event_id = event_id!("$h29iv0s8:example.com");
    let replied_to_event_id = event_id!("$foo:bar.com");

    mock.mock_authenticated_media_config().ok_default().mount().await;

    mock.mock_room_send()
        .body_matches_partial_json(json!({
            "m.relates_to": {
                "m.in_reply_to": {
                    "event_id": replied_to_event_id
                },
            }
        }))
        .ok(expected_event_id)
        .mock_once()
        .mount()
        .await;

    let f = EventFactory::new();
    mock.mock_room_event()
        .match_event_id()
        .ok(f
            .text_msg("Send me your attachments")
            .sender(*ALICE)
            .event_id(replied_to_event_id)
            .into())
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
                .mentions(Some(Mentions::with_user_ids([owned_user_id!("@user:localhost")])))
                .reply(Some(Reply {
                    event_id: replied_to_event_id.into(),
                    enforce_thread: EnforceThread::Unthreaded,
                })),
        )
        .await
        .unwrap();

    assert_eq!(expected_event_id, response.event_id);
}

#[async_test]
async fn test_room_attachment_start_thread() {
    let mock = MatrixMockServer::new().await;

    let expected_event_id = event_id!("$h29iv0s8:example.com");
    let replied_to_event_id = event_id!("$foo:bar.com");

    mock.mock_authenticated_media_config().ok_default().mount().await;

    mock.mock_room_send()
        .body_matches_partial_json(json!({
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": replied_to_event_id,
                "m.in_reply_to": {
                    "event_id": replied_to_event_id
                },
                "is_falling_back": true
            },
        }))
        .ok(expected_event_id)
        .mock_once()
        .mount()
        .await;

    let f = EventFactory::new();
    mock.mock_room_event()
        .match_event_id()
        .ok(f
            .text_msg("Send me your attachments")
            .sender(*ALICE)
            .event_id(replied_to_event_id)
            .into())
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
                .mentions(Some(Mentions::with_user_ids([owned_user_id!("@user:localhost")])))
                .reply(Some(Reply {
                    event_id: replied_to_event_id.into(),
                    enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
                })),
        )
        .await
        .unwrap();

    assert_eq!(expected_event_id, response.event_id);
}

#[async_test]
async fn test_room_attachment_reply_on_thread_as_reply() {
    let mock = MatrixMockServer::new().await;

    let expected_event_id = event_id!("$h29iv0s8:example.com");
    let thread_root_event_id = event_id!("$bar:foo.com");
    let replied_to_event_id = event_id!("$foo:bar.com");

    mock.mock_authenticated_media_config().ok_default().mount().await;

    mock.mock_room_send()
        .body_matches_partial_json(json!({
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": thread_root_event_id,
                "m.in_reply_to": {
                    "event_id": replied_to_event_id
                },
            },
        }))
        .ok(expected_event_id)
        .mock_once()
        .mount()
        .await;

    let f = EventFactory::new();
    mock.mock_room_event()
        .match_event_id()
        .ok(f
            .text_msg("Send me your attachments")
            .sender(*ALICE)
            .event_id(replied_to_event_id)
            .in_thread(thread_root_event_id, thread_root_event_id)
            .into())
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
                .mentions(Some(Mentions::with_user_ids([owned_user_id!("@user:localhost")])))
                .reply(Some(Reply {
                    event_id: replied_to_event_id.into(),
                    enforce_thread: EnforceThread::Threaded(ReplyWithinThread::Yes),
                })),
        )
        .await
        .unwrap();

    assert_eq!(expected_event_id, response.event_id);
}

#[async_test]
async fn test_room_attachment_reply_forwarding_thread() {
    let mock = MatrixMockServer::new().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;

    let expected_event_id = event_id!("$h29iv0s8:example.com");
    let thread_root_event_id = event_id!("$bar:foo.com");
    let replied_to_event_id = event_id!("$foo:bar.com");

    mock.mock_room_send()
        .body_matches_partial_json(json!({
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": thread_root_event_id,
                "m.in_reply_to": {
                    "event_id": replied_to_event_id
                },
                "is_falling_back": true
            },
        }))
        .ok(expected_event_id)
        .mock_once()
        .mount()
        .await;

    let f = EventFactory::new();
    mock.mock_room_event()
        .match_event_id()
        .ok(f
            .text_msg("Send me your attachments")
            .sender(*ALICE)
            .event_id(replied_to_event_id)
            .in_thread(thread_root_event_id, thread_root_event_id)
            .into())
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
                .mentions(Some(Mentions::with_user_ids([owned_user_id!("@user:localhost")])))
                .reply(Some(Reply {
                    event_id: replied_to_event_id.into(),
                    enforce_thread: EnforceThread::MaybeThreaded,
                })),
        )
        .await
        .unwrap();

    assert_eq!(expected_event_id, response.event_id);
}

#[async_test]
async fn test_room_attachment_send_is_animated() {
    let mock = MatrixMockServer::new().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;

    let expected_event_id = event_id!("$h29iv0s8:example.com");
    mock.mock_room_send()
        .body_matches_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
                "org.matrix.msc4230.is_animated": false,
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
            is_animated: Some(false),
            ..Default::default()
        }))
        .caption(Some(TextMessageEventContent::plain("image caption")));

    let response = room
        .send_attachment("image.jpg", &mime::IMAGE_JPEG, b"Hello world".to_vec(), config)
        .await
        .unwrap();

    assert_eq!(expected_event_id, response.event_id)
}
