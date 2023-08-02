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
use axum::{routing::get, Json};
use axum_extra::response::ErasedJson;
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

use crate::axum::{logged_in_client, ResponseVar, RouterExt};

#[async_test]
async fn back_pagination() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let sync_builder = SyncResponseBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let get_messages_response_var = ResponseVar::new();
    let client = logged_in_client(
        axum::Router::new()
            .route(
                "/_matrix/client/v3/rooms/:room_id/messages",
                get(get_messages_response_var.clone()),
            )
            .mock_sync_responses(sync_builder.clone()),
    )
    .await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    client.sync_once(sync_settings.clone()).await.unwrap();

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let (_, mut timeline_stream) = timeline.subscribe().await;
    let mut back_pagination_status = timeline.back_pagination_status();

    let paginate = async {
        get_messages_response_var.set(ErasedJson::new(&*test_json::ROOM_MESSAGES_BATCH_1));
        timeline.paginate_backwards(PaginationOptions::single_request(10)).await.unwrap();
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

    get_messages_response_var.set(Json(json!( {
        // Usually there would be a few events here, but we just want to test
        // that the timeline start item is added when there is no end token
        "chunk": [],
        "start": "t47409-4357353_219380_26003_2269"
    })));
    timeline.paginate_backwards(PaginationOptions::single_request(10)).await.unwrap();
    assert_next_eq!(back_pagination_status, BackPaginationStatus::TimelineStartReached);
}

#[async_test]
async fn back_pagination_highlighted() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let sync_builder = SyncResponseBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let client = logged_in_client(
        axum::Router::new()
            .route(
                "/_matrix/client/v3/rooms/:room_id/messages",
                get(Json(json!({
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
                }))),
            )
            .mock_sync_responses(sync_builder.clone()),
    )
    .await;

    sync_builder
        // We need the member event and power levels locally so the push rules processor works.
        .add_joined_room(
            JoinedRoomBuilder::new(room_id)
                .add_state_event(StateTestEvent::Member)
                .add_state_event(StateTestEvent::PowerLevels),
        );
    client.sync_once(sync_settings.clone()).await.unwrap();

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let (_, mut timeline_stream) = timeline.subscribe().await;

    timeline.paginate_backwards(PaginationOptions::single_request(10)).await.unwrap();

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
