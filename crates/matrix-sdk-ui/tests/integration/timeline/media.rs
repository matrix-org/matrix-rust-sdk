// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{fs::File, io::Write as _, path::PathBuf, time::Duration};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
#[cfg(feature = "unstable-msc4274")]
use matrix_sdk::attachment::{AttachmentInfo, BaseFileInfo};
use matrix_sdk::{
    assert_let_timeout,
    attachment::Thumbnail,
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    send_queue::AbstractProgress,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{
    ALICE, JoinedRoomBuilder, TestResult, async_test, event_factory::EventFactory,
};
use matrix_sdk_ui::timeline::{
    AttachmentConfig, AttachmentSource, EventSendState, MediaUploadProgress, RoomExt, TimelineFocus,
};
#[cfg(feature = "unstable-msc4274")]
use matrix_sdk_ui::timeline::{GalleryConfig, GalleryItemInfo};
#[cfg(feature = "unstable-msc4274")]
use ruma::events::room::message::GalleryItemType;
#[cfg(feature = "unstable-msc4274")]
use ruma::owned_mxc_uri;
use ruma::{
    event_id,
    events::room::{
        MediaSource,
        message::{MessageType, TextMessageEventContent},
    },
    room_id, uint,
};
use serde_json::json;
use stream_assert::assert_pending;
use tempfile::TempDir;
use wiremock::ResponseTemplate;

fn create_temporary_file(filename: &str) -> anyhow::Result<(TempDir, PathBuf)> {
    let tmp_dir = TempDir::new()?;
    let file_path = tmp_dir.path().join(filename);
    let mut file = File::create(&file_path)?;
    file.write_all(b"hello world")?;
    Ok((tmp_dir, file_path))
}

fn get_filename_and_caption(msg: &MessageType) -> (&str, Option<&str>) {
    match msg {
        MessageType::File(event) => (event.filename(), event.caption()),
        MessageType::Image(event) => (event.filename(), event.caption()),
        MessageType::Video(event) => (event.filename(), event.caption()),
        MessageType::Audio(event) => (event.filename(), event.caption()),
        _ => panic!("unexpected message type"),
    }
}

#[async_test]
async fn test_send_attachment_from_file() -> TestResult {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = mock.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await?;

    let (items, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    assert!(items.is_empty());

    let event_id = event_id!("$event");
    let f = EventFactory::new();
    mock.sync_room(
        &client,
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(event_id)),
    )
    .await;

    // Sanity check.
    assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
    assert_let!(Some(msg) = item.content().as_message());
    assert_eq!(msg.body(), "hello");

    // No other updates.
    assert_pending!(timeline_stream);

    // Store a file in a temporary directory.
    let (_tmp_dir, file_path) = create_temporary_file("test.bin")?;

    // Set up mocks for the file upload.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)).set_body_json(
            json!({
              "content_uri": "mxc://sdk.rs/media"
            }),
        ))
        .mock_once()
        .mount()
        .await;

    mock.mock_room_send().ok(event_id!("$media")).mock_once().mount().await;

    // Queue sending of an attachment in the thread.
    let thread_timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread { root_event_id: event_id.to_owned() })
        .build()
        .await?;
    let config = AttachmentConfig {
        caption: Some(TextMessageEventContent::plain("caption")),
        ..Default::default()
    };
    thread_timeline.send_attachment(&file_path, mime::TEXT_PLAIN, config).use_send_queue().await?;

    {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));
        assert_let!(Some(msg) = item.content().as_message());

        // Body is the caption, because there's both a caption and filename.
        assert_eq!(msg.body(), "caption");
        assert_eq!(get_filename_and_caption(msg.msgtype()), ("test.bin", Some("caption")));

        // The URI refers to the local cache.
        assert_let!(MessageType::File(file) = msg.msgtype());
        assert_let!(MediaSource::Plain(uri) = &file.source);
        assert!(uri.to_string().contains("localhost"));

        // The message should be considered part of the thread.
        let aggregated = item.content().as_msglike().unwrap();
        assert!(aggregated.is_threaded());
    }

    // The media upload finishes.
    let (final_index, final_progress) = {
        assert_let_timeout!(
            Duration::from_secs(3),
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_let!(Some(msg) = item.content().as_message());
        assert_let!(
            Some(EventSendState::NotSentYet {
                progress: Some(MediaUploadProgress { index, progress })
            }) = item.send_state()
        );
        assert_eq!(*index, 0);
        assert_eq!(progress.current, progress.total);
        assert_eq!(get_filename_and_caption(msg.msgtype()), ("test.bin", Some("caption")));

        // The URI still refers to the local cache.
        assert_let!(MessageType::File(file) = msg.msgtype());
        assert_let!(MediaSource::Plain(uri) = &file.source);
        assert!(uri.to_string().contains("localhost"));

        (*index, *progress)
    };

    // Eventually, the media is updated with the final MXC IDs…
    {
        assert_let_timeout!(
            Duration::from_secs(3),
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_let!(Some(msg) = item.content().as_message());
        assert_let!(
            Some(EventSendState::NotSentYet {
                progress: Some(MediaUploadProgress { index, progress })
            }) = item.send_state()
        );
        assert_eq!(*index, final_index);
        assert_eq!(progress.current, final_progress.current);
        assert_eq!(progress.total, final_progress.total);
        assert_eq!(get_filename_and_caption(msg.msgtype()), ("test.bin", Some("caption")));

        // The URI now refers to the final MXC URI.
        assert_let!(MessageType::File(file) = msg.msgtype());
        assert_let!(MediaSource::Plain(uri) = &file.source);
        assert_eq!(uri.to_string(), "mxc://sdk.rs/media");
    }

    // And eventually the event itself is sent.
    {
        assert_let_timeout!(
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_matches!(item.send_state(), Some(EventSendState::Sent{ event_id }) => {
            assert_eq!(event_id, event_id!("$media"));
        });

        // Since it's sent, it's inserted in the Event Cache, and becomes a
        // remote event.
        assert_let_timeout!(Some(VectorDiff::Remove { index: 1 }) = timeline_stream.next());
        assert_let_timeout!(
            Some(VectorDiff::Insert { index: 1, value: remote_event }) = timeline_stream.next()
        );
        assert_eq!(remote_event.event_id().unwrap(), event_id!("$media"));
    }

    // That's all, folks!
    assert_pending!(timeline_stream);
    Ok(())
}

#[async_test]
async fn test_send_attachment_from_bytes() -> TestResult {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;
    client.send_queue().enable_upload_progress(true);

    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = mock.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await?;

    let (items, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    assert!(items.is_empty());

    let f = EventFactory::new();
    mock.sync_room(
        &client,
        JoinedRoomBuilder::new(room_id).add_timeline_event(f.text_msg("hello").sender(&ALICE)),
    )
    .await;

    // Sanity check.
    assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
    assert_let!(Some(msg) = item.content().as_message());
    assert_eq!(msg.body(), "hello");

    // No other updates.
    assert_pending!(timeline_stream);

    // The data of the file.
    let filename = "test.bin";
    let bytes = b"hello world".to_vec();
    let size = bytes.len();
    let source = AttachmentSource::Data { bytes, filename: filename.to_owned() };

    // Set up mocks for the file upload.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)).set_body_json(
            json!({
              "content_uri": "mxc://sdk.rs/media"
            }),
        ))
        .mock_once()
        .mount()
        .await;

    mock.mock_room_send().ok(event_id!("$media")).mock_once().mount().await;

    // Queue sending of an attachment.
    let config = AttachmentConfig {
        caption: Some(TextMessageEventContent::plain("caption")),
        ..Default::default()
    };
    timeline.send_attachment(source, mime::TEXT_PLAIN, config).use_send_queue().await?;

    {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));
        assert_let!(Some(msg) = item.content().as_message());

        // Body is the caption, because there's both a caption and filename.
        assert_eq!(msg.body(), "caption");
        assert_eq!(get_filename_and_caption(msg.msgtype()), (filename, Some("caption")));

        // The URI refers to the local cache.
        assert_let!(MessageType::File(file) = msg.msgtype());
        assert_let!(MediaSource::Plain(uri) = &file.source);
        assert!(uri.to_string().contains("localhost"));
    }

    // The media upload progress is being reported and eventually the upload
    // finishes.
    {
        let mut prev_progress: Option<AbstractProgress> = None;

        loop {
            assert_let_timeout!(
                Duration::from_secs(3),
                Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
            );

            // The caption is still correct.
            assert_let!(Some(msg) = item.content().as_message());
            assert_eq!(get_filename_and_caption(msg.msgtype()), (filename, Some("caption")));

            assert_let!(Some(EventSendState::NotSentYet { progress }) = item.send_state());

            assert_let!(Some(MediaUploadProgress { index, progress }) = progress);

            // We're only uploading a single file.
            assert_eq!(*index, 0);

            // The progress is reported in units of the unencrypted file size.
            assert!(progress.current <= progress.total);
            assert_eq!(progress.total, size);

            // The progress only increases.
            if let Some(prev_progress) = prev_progress {
                assert!(progress.current >= prev_progress.current);
            }
            prev_progress = Some(*progress);

            assert_let!(MessageType::File(file) = msg.msgtype());
            assert_let!(MediaSource::Plain(uri) = &file.source);

            // Check if the upload finished and the URI now refers to the final MXC URI.
            if progress.current == progress.total && *uri == "mxc://sdk.rs/media" {
                break;
            }

            // Otherwise, the URI still refers to the local cache.
            assert!(uri.to_string().contains("localhost"));
        }
    }

    // And eventually the event itself is sent.
    {
        assert_let_timeout!(
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_matches!(item.send_state(), Some(EventSendState::Sent{ event_id }) => {
            assert_eq!(event_id, event_id!("$media"));
        });

        // Since it's sent, it's inserted in the Event Cache, and becomes a
        // remote event.
        assert_let_timeout!(Some(VectorDiff::Remove { index: 1 }) = timeline_stream.next());
        assert_let_timeout!(
            Some(VectorDiff::Insert { index: 1, value: remote_event }) = timeline_stream.next()
        );
        assert_eq!(remote_event.event_id().unwrap(), event_id!("$media"));
    }

    // That's all, folks!
    assert_pending!(timeline_stream);
    Ok(())
}

#[async_test]
async fn test_send_media_with_thumbnail() -> TestResult {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = mock.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await?;

    let (items, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    assert!(items.is_empty());
    assert_pending!(timeline_stream);

    // The data of the file.
    let thumbnail_data = b"hello world".to_vec();

    // A mock to upload the thumbnail.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://sdk.rs/thumbnail"
        })))
        .mock_once()
        .mount()
        .await;

    // A mock to upload the media file.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://sdk.rs/media"
        })))
        .mock_once()
        .mount()
        .await;

    // A mock for sending the media event.
    mock.mock_room_send().ok(event_id!("$media")).mock_once().mount().await;

    // Send an attachment with a thumbnail.
    timeline
        .send_attachment(
            AttachmentSource::Data { bytes: vec![1, 2, 3], filename: "image.png".to_owned() },
            mime::IMAGE_PNG,
            AttachmentConfig {
                thumbnail: Some(Thumbnail {
                    data: thumbnail_data.clone(),
                    content_type: mime::IMAGE_PNG,
                    height: uint!(13),
                    width: uint!(37),
                    size: uint!(42),
                }),
                ..Default::default()
            },
        )
        .use_send_queue()
        .await
        .unwrap();

    {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));
        assert_let!(Some(msg) = item.content().as_message());

        // Message is an image, as intended.
        assert_let!(MessageType::Image(content) = msg.msgtype());
        assert_eq!(content.filename(), "image.png");

        // At the moment, it's stored in the media cache store, so its media source
        // references a local URI.
        assert_let!(MediaSource::Plain(uri) = &content.source);
        assert!(uri.to_string().contains("localhost"));

        // The thumbnail media source also has a local URI.
        let media_info = content.info.as_ref().unwrap();
        assert_let!(Some(MediaSource::Plain(uri)) = &media_info.thumbnail_source);
        assert!(uri.to_string().contains("localhost"));

        let thumbnail = media_info.thumbnail_info.as_ref().unwrap();
        // Sanity checks, although we're not quite interested in these values here.
        assert_eq!(thumbnail.height.unwrap(), uint!(13));
        assert_eq!(thumbnail.width.unwrap(), uint!(37));
        assert_eq!(thumbnail.size.unwrap(), uint!(42));
        assert_eq!(thumbnail.mimetype.as_deref().unwrap(), "image/png");
    }

    // First, we get an update because the thumbnail has been uploaded.
    {
        assert_let_timeout!(
            Duration::from_secs(2),
            Some(VectorDiff::Set { index: 0, value: item }) = timeline_stream.next()
        );
        assert_let!(
            Some(EventSendState::NotSentYet {
                progress: Some(MediaUploadProgress { index, progress })
            }) = item.send_state()
        );
        assert_let!(Some(msg) = item.content().as_message());

        // Message is an image, as intended, and MXC URIs are still local.
        assert_let!(MessageType::Image(content) = msg.msgtype());
        assert_eq!(content.filename(), "image.png");

        assert_let!(MediaSource::Plain(uri) = &content.source);
        assert!(uri.to_string().contains("localhost"));

        let media_info = content.info.as_ref().unwrap();
        assert_let!(Some(MediaSource::Plain(uri)) = &media_info.thumbnail_source);
        assert!(uri.to_string().contains("localhost"));

        // The upload has of the thumbnail has finished, but not the media yet.
        assert_eq!(*index, 0);
        assert_eq!(progress.current, 1);
        assert_eq!(progress.total, 1);
    }

    // Then, the media gets uploaded too.
    {
        assert_let_timeout!(
            Duration::from_secs(2),
            Some(VectorDiff::Set { index: 0, value: item }) = timeline_stream.next()
        );
        assert_let!(
            Some(EventSendState::NotSentYet {
                progress: Some(MediaUploadProgress { index, progress })
            }) = item.send_state()
        );
        assert_let!(Some(msg) = item.content().as_message());

        // Message is an image, as intended, and MXC URIs are still local.
        assert_let!(MessageType::Image(content) = msg.msgtype());
        assert_eq!(content.filename(), "image.png");

        assert_let!(MediaSource::Plain(uri) = &content.source);
        assert!(uri.to_string().contains("localhost"));

        let media_info = content.info.as_ref().unwrap();
        assert_let!(Some(MediaSource::Plain(uri)) = &media_info.thumbnail_source);
        assert!(uri.to_string().contains("localhost"));

        assert_eq!(*index, 0);
        assert_eq!(progress.current, 1);
        assert_eq!(progress.total, 1);
    }

    // Eventually, the media is updated with the final MXC IDs…
    {
        assert_let_timeout!(
            Duration::from_secs(3),
            Some(VectorDiff::Set { index: 0, value: item }) = timeline_stream.next()
        );
        assert_let!(Some(msg) = item.content().as_message());

        // Message is an image, as intended, and both MXC URIs are not local anymore.
        assert_let!(MessageType::Image(content) = msg.msgtype());
        assert_eq!(content.filename(), "image.png");

        assert_let!(MediaSource::Plain(uri) = &content.source);
        assert_eq!(uri.to_string(), "mxc://sdk.rs/media");

        let media_info = content.info.as_ref().unwrap();
        assert_let!(Some(MediaSource::Plain(uri)) = &media_info.thumbnail_source);
        assert_eq!(uri.to_string(), "mxc://sdk.rs/thumbnail");

        assert_let!(Some(EventSendState::NotSentYet { .. }) = item.send_state());
    }

    // And eventually the event itself is sent.
    {
        assert_let_timeout!(
            Some(VectorDiff::Set { index: 0, value: item }) = timeline_stream.next()
        );
        assert_matches!(item.send_state(), Some(EventSendState::Sent{ event_id }) => {
            assert_eq!(event_id, event_id!("$media"));
        });

        assert_let!(Some(msg) = item.content().as_message());
        assert_let!(MessageType::Image(content) = msg.msgtype());
        assert_let!(
            Some(MediaSource::Plain(thumbnail_uri)) =
                &content.info.as_ref().unwrap().thumbnail_source
        );
        assert_eq!(thumbnail_uri.to_string(), "mxc://sdk.rs/thumbnail");

        // Now, getting the thumbnail should be a cache hit and not require an extra
        // endpoint to be set up; the test will fail with a 404 error if there's
        // a cache miss.
        let use_cache = true;
        let retrieved_thumbnail_data = client
            .media()
            .get_media_content(
                &MediaRequestParameters {
                    source: MediaSource::Plain(thumbnail_uri.clone()),
                    format: MediaFormat::File,
                },
                use_cache,
            )
            .await
            .unwrap();
        // And the thumbnail data matches what we've sent.
        assert_eq!(retrieved_thumbnail_data, thumbnail_data);

        // Another way to get the thumbnail data is to request a thumbnail with the same
        // sizes as the ones we've sent during the upload.
        let retrieved_thumbnail_data = client
            .media()
            .get_media_content(
                &MediaRequestParameters {
                    source: MediaSource::Plain(thumbnail_uri.clone()),
                    format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(
                        uint!(37),
                        uint!(13),
                    )),
                },
                use_cache,
            )
            .await
            .unwrap();
        // And the thumbnail data still matches what we've sent.
        assert_eq!(retrieved_thumbnail_data, thumbnail_data);

        // Since it's sent, it's inserted in the Event Cache, and reinserted as a
        // remote event.
        assert_let_timeout!(Some(VectorDiff::Remove { index: 0 }) = timeline_stream.next());
        assert_let_timeout!(Some(VectorDiff::PushFront { value: item }) = timeline_stream.next());
        assert_eq!(item.event_id().unwrap(), event_id!("$media"));
    }

    // That's all, folks!
    assert_pending!(timeline_stream);
    Ok(())
}

#[cfg(feature = "unstable-msc4274")]
#[async_test]
async fn test_send_gallery_from_bytes() -> TestResult {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = mock.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await?;

    let (items, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    assert!(items.is_empty());

    let f = EventFactory::new();
    mock.sync_room(
        &client,
        JoinedRoomBuilder::new(room_id).add_timeline_event(
            f.gallery(
                "check out my favourite gifs".to_owned(),
                "rickroll.gif".to_owned(),
                owned_mxc_uri!("mxc://sdk.rs/rickroll"),
            )
            .sender(&ALICE),
        ),
    )
    .await;

    // Sanity check.
    assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
    assert_let!(Some(msg) = item.content().as_message());
    assert_eq!(msg.body(), "check out my favourite gifs");

    // No other updates.
    assert_pending!(timeline_stream);

    // The data of the file.
    let filename = "test.bin";
    let data = b"hello world".to_vec();

    // Set up mocks for the file upload.
    mock.mock_upload()
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)).set_body_json(
            json!({
              "content_uri": "mxc://sdk.rs/media"
            }),
        ))
        .mock_once()
        .mount()
        .await;

    mock.mock_room_send().ok(event_id!("$media")).mock_once().mount().await;

    // Queue sending of a gallery.
    let gallery = GalleryConfig::new()
        .caption(Some(TextMessageEventContent::plain("caption")))
        .add_item(GalleryItemInfo {
            source: AttachmentSource::Data { bytes: data, filename: filename.to_owned() },
            content_type: mime::TEXT_PLAIN,
            attachment_info: AttachmentInfo::File(BaseFileInfo { size: None }),
            caption: Some(TextMessageEventContent::plain("item caption")),
            thumbnail: None,
        });
    timeline.send_gallery(gallery).await?;

    {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));
        assert_let!(Some(msg) = item.content().as_message());

        // Body matches gallery caption.
        assert_eq!(msg.body(), "caption");

        // Message is gallery of expected length
        assert_let!(MessageType::Gallery(content) = msg.msgtype());
        assert_eq!(1, content.itemtypes.len());
        assert_let!(GalleryItemType::File(file) = content.itemtypes.first().unwrap());

        // Item has filename and caption
        assert_eq!(filename, file.filename());
        assert_eq!(Some("item caption"), file.caption());

        // The URI refers to the local cache.
        assert_let!(MediaSource::Plain(uri) = &file.source);
        assert!(uri.to_string().contains("localhost"));
    }

    // The media upload finishes.
    let (final_index, final_progress) = {
        assert_let_timeout!(
            Duration::from_secs(3),
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_let!(
            Some(EventSendState::NotSentYet {
                progress: Some(MediaUploadProgress { index, progress })
            }) = item.send_state()
        );
        assert_let!(Some(msg) = item.content().as_message());

        // The upload has finished.
        assert_eq!(*index, 0);
        assert_eq!(progress.current, progress.total);

        // Body matches gallery caption.
        assert_eq!(msg.body(), "caption");

        // Message is gallery of expected length
        assert_let!(MessageType::Gallery(content) = msg.msgtype());
        assert_eq!(1, content.itemtypes.len());
        assert_let!(GalleryItemType::File(file) = content.itemtypes.first().unwrap());

        // Item has filename and caption
        assert_eq!(filename, file.filename());
        assert_eq!(Some("item caption"), file.caption());

        // The URI still refers to the local cache.
        assert_let!(MediaSource::Plain(uri) = &file.source);
        assert!(uri.to_string().contains("localhost"));

        (*index, *progress)
    };

    // Eventually, the media is updated with the final MXC IDs…
    {
        assert_let_timeout!(
            Duration::from_secs(3),
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_let!(Some(msg) = item.content().as_message());
        assert_let!(
            Some(EventSendState::NotSentYet {
                progress: Some(MediaUploadProgress { index, progress })
            }) = item.send_state()
        );
        assert_eq!(*index, final_index);
        assert_eq!(progress.current, final_progress.current);
        assert_eq!(progress.total, final_progress.total);

        // Message is gallery of expected length
        assert_let!(MessageType::Gallery(content) = msg.msgtype());
        assert_eq!(1, content.itemtypes.len());
        assert_let!(GalleryItemType::File(file) = content.itemtypes.first().unwrap());

        // Item has filename and caption
        assert_eq!(filename, file.filename());
        assert_eq!(Some("item caption"), file.caption());

        // The URI now refers to the final MXC URI.
        assert_let!(MediaSource::Plain(uri) = &file.source);
        assert_eq!(uri.to_string(), "mxc://sdk.rs/media");
    }

    // And eventually the event itself is sent.
    {
        assert_let_timeout!(
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_matches!(item.send_state(), Some(EventSendState::Sent{ event_id }) => {
            assert_eq!(event_id, event_id!("$media"));
        });

        // Since it's sent, it's inserted in the Event Cache, and becomes a
        // remote event.
        assert_let_timeout!(Some(VectorDiff::Remove { index: 1 }) = timeline_stream.next());
        assert_let_timeout!(
            Some(VectorDiff::Insert { index: 1, value: remote_event }) = timeline_stream.next()
        );
        assert_eq!(remote_event.event_id().unwrap(), event_id!("$media"));
    }

    // That's all, folks!
    assert_pending!(timeline_stream);
    Ok(())
}

#[async_test]
async fn test_react_to_local_media() -> TestResult {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    // Disable the sending queue, to simulate offline mode.
    client.send_queue().set_enabled(false).await;

    mock.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = mock.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await?;

    let (items, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    assert!(items.is_empty());
    assert_pending!(timeline_stream);

    // Store a file in a temporary directory.
    let (_tmp_dir, file_path) = create_temporary_file("test.bin")?;

    // Queue sending of an attachment (no captions).
    let config = AttachmentConfig::default();
    timeline.send_attachment(&file_path, mime::TEXT_PLAIN, config).use_send_queue().await?;

    let item_id = {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_let!(Some(msg) = item.content().as_message());
        assert_eq!(get_filename_and_caption(msg.msgtype()), ("test.bin", None));

        // The item starts with no reactions.
        assert!(item.content().reactions().cloned().unwrap_or_default().is_empty());

        item.identifier()
    };

    // Add a reaction to the file media event.
    timeline.toggle_reaction(&item_id, "🤪").await?;

    assert_let_timeout!(Some(VectorDiff::Set { index: 0, value: item }) = timeline_stream.next());
    assert_let!(Some(msg) = item.content().as_message());
    assert_eq!(get_filename_and_caption(msg.msgtype()), ("test.bin", None));

    // There's a reaction for the current user for the given emoji.
    let reactions = item.content().reactions().cloned().unwrap_or_default();
    let own_user_id = client.user_id().unwrap();
    reactions.get("🤪").unwrap().get(own_user_id).unwrap();

    // That's all, folks!
    assert_pending!(timeline_stream);
    Ok(())
}
