// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::time::Duration;

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{
    async_test, EventBuilder, JoinedRoomBuilder, SyncResponseBuilder, ALICE, BOB,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineItemContent};
use ruma::{
    event_id,
    events::room::message::{MessageType, ReplacementMetadata, RoomMessageEventContent},
    room_id,
};

use crate::{logged_in_client, mock_sync};

#[async_test]
async fn edit() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$msda7m:localhost");
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        event_builder.make_sync_message_event_with_id(
            &ALICE,
            event_id,
            RoomMessageEventContent::text_plain("hello"),
        ),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let _day_divider = assert_matches!(
        timeline_stream.next().await,
        Some(VectorDiff::PushBack { value }) => value
    );
    let first = assert_matches!(
        timeline_stream.next().await,
        Some(VectorDiff::PushBack { value }) => value
    );
    let msg = assert_matches!(
        first.as_event().unwrap().content(),
        TimelineItemContent::Message(msg) => msg
    );
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert_matches!(msg.in_reply_to(), None);
    assert!(!msg.is_edited());

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(event_builder.make_sync_message_event(
                &BOB,
                RoomMessageEventContent::text_html("Test", "<em>Test</em>"),
            ))
            .add_timeline_event(
                event_builder.make_sync_message_event(
                    &ALICE,
                    RoomMessageEventContent::text_plain("hi").make_replacement(
                        ReplacementMetadata::new(event_id.to_owned(), None),
                        None,
                    ),
                ),
            ),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let second = assert_matches!(timeline_stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = second.as_event().unwrap();
    assert!(item.event_id().is_some());
    assert!(!item.is_own());
    assert!(item.original_json().is_some());

    let msg = assert_matches!(item.content(), TimelineItemContent::Message(msg) => msg);
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert_matches!(msg.in_reply_to(), None);
    assert!(!msg.is_edited());

    let edit = assert_matches!(
        timeline_stream.next().await,
        Some(VectorDiff::Set { index: 1, value }) => value
    );
    let edited = assert_matches!(
        edit.as_event().unwrap().content(),
        TimelineItemContent::Message(msg) => msg
    );
    let text = assert_matches!(edited.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "hi");
    assert_matches!(edited.in_reply_to(), None);
    assert!(edited.is_edited());
}
