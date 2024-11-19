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

use std::{fs::File, io::Write as _, time::Duration};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{
    assert_let_timeout,
    attachment::AttachmentConfig,
    test_utils::{events::EventFactory, mocks::MatrixMockServer},
};
use matrix_sdk_test::{async_test, JoinedRoomBuilder, ALICE};
use matrix_sdk_ui::timeline::{EventSendState, RoomExt, TimelineItemContent};
use ruma::{
    event_id,
    events::room::{message::MessageType, MediaSource},
    room_id,
};
use serde_json::json;
use tempfile::TempDir;
use tokio::time::sleep;
use wiremock::ResponseTemplate;

#[async_test]
async fn test_send_attachment() {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

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
    assert_let!(TimelineItemContent::Message(msg) = item.content());
    assert_eq!(msg.body(), "hello");

    // No other updates.
    assert!(timeline_stream.next().now_or_never().is_none());

    // Store a file in a temporary directory.
    let tmp_dir = TempDir::new().unwrap();

    let file_path = tmp_dir.path().join("test.bin");
    {
        let mut file = File::create(&file_path).unwrap();
        file.write_all(b"hello world").unwrap();
    }

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
    timeline.send_attachment(&file_path, mime::TEXT_PLAIN, config).use_send_queue().await.unwrap();

    {
        assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
        assert_matches!(item.send_state(), Some(EventSendState::NotSentYet));

        assert_let!(TimelineItemContent::Message(msg) = item.content());

        // Body is the caption, because there's both a caption and filename.
        assert_eq!(msg.body(), "caption");
        assert_let!(MessageType::File(file) = msg.msgtype());
        assert_eq!(file.filename(), "test.bin");
        assert_eq!(file.caption(), Some("caption"));

        // The URI refers to the local cache.
        assert_let!(MediaSource::Plain(uri) = &file.source);
        assert!(uri.to_string().contains("localhost"));
    }

    // Eventually, the media is updated with the final MXC IDs…
    sleep(Duration::from_secs(2)).await;

    {
        assert_let_timeout!(
            Some(VectorDiff::Set { index: 1, value: item }) = timeline_stream.next()
        );
        assert_let!(TimelineItemContent::Message(msg) = item.content());

        assert_let!(MessageType::File(file) = msg.msgtype());
        assert_eq!(file.filename(), "test.bin");
        assert_eq!(file.caption(), Some("caption"));

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
    assert!(timeline_stream.next().now_or_never().is_none());
}
