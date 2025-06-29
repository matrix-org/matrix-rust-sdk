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
    attachment::AttachmentConfig,
    room::reply::{EnforceThread, Reply},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{AttachmentSource, EventSendState, RoomExt};
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
        message::{MessageType, ReplyWithinThread},
    },
    room_id,
};
use serde_json::json;
use stream_assert::assert_pending;
use tempfile::TempDir;
use wiremock::ResponseTemplate;

fn create_temporary_file(filename: &str) -> (TempDir, PathBuf) {
    let tmp_dir = TempDir::new().unwrap();
    let file_path = tmp_dir.path().join(filename);
    let mut file = File::create(&file_path).unwrap();
    file.write_all(b"hello world").unwrap();
    (tmp_dir, file_path)
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
async fn test_send_attachment_from_file() {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = mock.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await.unwrap();

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
    let (_tmp_dir, file_path) = create_temporary_file("test.bin");

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
    let config = AttachmentConfig::new().caption(Some("caption".to_owned())).reply(Some(Reply {
        event_id: event_id.to_owned(),
        enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
    }));
    timeline.send_attachment(&file_path, mime::TEXT_PLAIN, config).use_send_queue().await.unwrap();

    {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));
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

    // Eventually, the media is updated with the final MXC IDs…
    {
        assert_let_timeout!(
            Duration::from_secs(3),
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_let!(Some(msg) = item.content().as_message());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));
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
    }

    // That's all, folks!
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_send_attachment_from_bytes() {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = mock.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await.unwrap();

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
    let source =
        AttachmentSource::Data { bytes: b"hello world".to_vec(), filename: filename.to_owned() };

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
    let config = AttachmentConfig::new().caption(Some("caption".to_owned()));
    timeline.send_attachment(source, mime::TEXT_PLAIN, config).use_send_queue().await.unwrap();

    {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));
        assert_let!(Some(msg) = item.content().as_message());

        // Body is the caption, because there's both a caption and filename.
        assert_eq!(msg.body(), "caption");
        assert_eq!(get_filename_and_caption(msg.msgtype()), (filename, Some("caption")));

        // The URI refers to the local cache.
        assert_let!(MessageType::File(file) = msg.msgtype());
        assert_let!(MediaSource::Plain(uri) = &file.source);
        assert!(uri.to_string().contains("localhost"));
    }

    // Eventually, the media is updated with the final MXC IDs…
    {
        assert_let_timeout!(
            Duration::from_secs(3),
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_let!(Some(msg) = item.content().as_message());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));
        assert_eq!(get_filename_and_caption(msg.msgtype()), (filename, Some("caption")));

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
    }

    // That's all, folks!
    assert_pending!(timeline_stream);
}

#[cfg(feature = "unstable-msc4274")]
#[async_test]
async fn test_send_gallery_from_bytes() {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    mock.mock_authenticated_media_config().ok_default().mount().await;
    mock.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = mock.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await.unwrap();

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
    let gallery =
        GalleryConfig::new().caption(Some("caption".to_owned())).add_item(GalleryItemInfo {
            source: AttachmentSource::Data { bytes: data, filename: filename.to_owned() },
            content_type: mime::TEXT_PLAIN,
            attachment_info: AttachmentInfo::File(BaseFileInfo { size: None }),
            caption: Some("item caption".to_owned()),
            formatted_caption: None,
            thumbnail: None,
        });
    timeline.send_gallery(gallery).await.unwrap();

    {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));
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

    // Eventually, the media is updated with the final MXC IDs…
    {
        assert_let_timeout!(
            Duration::from_secs(3),
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_let!(Some(msg) = item.content().as_message());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

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
    }

    // That's all, folks!
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_react_to_local_media() {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    // Disable the sending queue, to simulate offline mode.
    client.send_queue().set_enabled(false).await;

    mock.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = mock.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await.unwrap();

    let (items, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    assert!(items.is_empty());
    assert_pending!(timeline_stream);

    // Store a file in a temporary directory.
    let (_tmp_dir, file_path) = create_temporary_file("test.bin");

    // Queue sending of an attachment (no captions).
    let config = AttachmentConfig::new();
    timeline.send_attachment(&file_path, mime::TEXT_PLAIN, config).use_send_queue().await.unwrap();

    let item_id = {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_let!(Some(msg) = item.content().as_message());
        assert_eq!(get_filename_and_caption(msg.msgtype()), ("test.bin", None));

        // The item starts with no reactions.
        assert!(item.content().reactions().cloned().unwrap_or_default().is_empty());

        item.identifier()
    };

    // Add a reaction to the file media event.
    timeline.toggle_reaction(&item_id, "🤪").await.unwrap();

    assert_let_timeout!(Some(VectorDiff::Set { index: 0, value: item }) = timeline_stream.next());
    assert_let!(Some(msg) = item.content().as_message());
    assert_eq!(get_filename_and_caption(msg.msgtype()), ("test.bin", None));

    // There's a reaction for the current user for the given emoji.
    let reactions = item.content().reactions().cloned().unwrap_or_default();
    let own_user_id = client.user_id().unwrap();
    reactions.get("🤪").unwrap().get(own_user_id).unwrap();

    // That's all, folks!
    assert_pending!(timeline_stream);
}
