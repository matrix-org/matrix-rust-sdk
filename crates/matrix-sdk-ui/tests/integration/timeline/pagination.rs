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

use std::{sync::Arc, time::Duration};

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::future::join;
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{
    async_test, test_json, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder,
};
use matrix_sdk_ui::timeline::{
    AnyOtherFullStateEventContent, BackPaginationStatus, PaginationOptions, RoomExt,
    TimelineItemContent, VirtualTimelineItem,
};
use ruma::{
    events::{room::message::MessageType, FullStateEventContent},
    room_id,
};
use serde_json::json;
use stream_assert::{assert_next_eq, assert_next_matches};
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync};

#[async_test]
async fn back_pagination() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let (_, mut timeline_stream) = timeline.subscribe().await;
    let mut back_pagination_status = timeline.back_pagination_status();

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_MESSAGES_BATCH_1))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    let paginate = async {
        timeline.paginate_backwards(PaginationOptions::single_request(10)).await.unwrap();
        server.reset().await;
    };
    let observe_paginating = async {
        assert_eq!(back_pagination_status.next().await, Some(BackPaginationStatus::Paginating));
    };
    join(paginate, observe_paginating).await;

    let day_divider = assert_next_matches!(
        timeline_stream,
        VectorDiff::PushFront { value } => value
    );
    assert_matches!(day_divider.as_virtual().unwrap(), VirtualTimelineItem::DayDivider(_));

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    let msg = assert_matches!(
        message.as_event().unwrap().content(),
        TimelineItemContent::Message(msg) => msg
    );
    let text = assert_matches!(msg.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "hello world");

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    let msg = assert_matches!(
        message.as_event().unwrap().content(),
        TimelineItemContent::Message(msg) => msg
    );
    let text = assert_matches!(msg.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "the world is big");

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    let state = assert_matches!(
        message.as_event().unwrap().content(),
        TimelineItemContent::OtherState(state) => state
    );
    assert_eq!(state.state_key(), "");
    let (content, prev_content) = assert_matches!(
        state.content(),
        AnyOtherFullStateEventContent::RoomName(
            FullStateEventContent::Original { content, prev_content }
        ) => (content, prev_content)
    );
    assert_eq!(content.name.as_ref().unwrap(), "New room name");
    assert_eq!(prev_content.as_ref().unwrap().name.as_ref().unwrap(), "Old room name");

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            // Usually there would be a few events here, but we just want to test
            // that the timeline start item is added when there is no end token
            "chunk": [],
            "start": "t47409-4357353_219380_26003_2269"
        })))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    timeline.paginate_backwards(PaginationOptions::single_request(10)).await.unwrap();
    assert_next_eq!(back_pagination_status, BackPaginationStatus::TimelineStartReached);
}

#[async_test]
async fn back_pagination_highlighted() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder
        // We need the member event and power levels locally so the push rules processor works.
        .add_joined_room(
            JoinedRoomBuilder::new(room_id)
                .add_state_event(StateTestEvent::Member)
                .add_state_event(StateTestEvent::PowerLevels),
        );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let response_json = json!({
        "chunk": [
          {
            "content": {
                "body": "hello",
                "msgtype": "m.text",
            },
            "event_id": "$msda7m0df9E9op3",
            "origin_server_ts": 152037280,
            "sender": "@example:localhost",
            "type": "m.room.message",
            "room_id": room_id,
          },
          {
            "content": {
                "body": "This room has been replaced",
                "replacement_room": "!newroom:localhost",
            },
            "event_id": "$foun39djjod0f",
            "origin_server_ts": 152039280,
            "sender": "@bob:localhost",
            "state_key": "",
            "type": "m.room.tombstone",
            "room_id": room_id,
          },
        ],
        "end": "t47409-4357353_219380_26003_2269",
        "start": "t392-516_47314_0_7_1_1_1_11444_1"
    });
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_json))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    timeline.paginate_backwards(PaginationOptions::single_request(10)).await.unwrap();
    server.reset().await;

    let day_divider = assert_next_matches!(
        timeline_stream,
        VectorDiff::PushFront { value } => value
    );
    assert_matches!(day_divider.as_virtual().unwrap(), VirtualTimelineItem::DayDivider(_));

    let first = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    let remote_event = first.as_event().unwrap();
    // Own events don't trigger push rules.
    assert!(!remote_event.is_highlighted());

    let second = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    let remote_event = second.as_event().unwrap();
    // `m.room.tombstone` should be highlighted by default.
    assert!(remote_event.is_highlighted());
}
