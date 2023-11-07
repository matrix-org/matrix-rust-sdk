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
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::future::{join, join3};
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{
    async_test, EventBuilder, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder, ALICE, BOB,
};
use matrix_sdk_ui::timeline::{
    AnyOtherFullStateEventContent, BackPaginationStatus, PaginationOptions, RoomExt,
    TimelineItemContent, VirtualTimelineItem,
};
use once_cell::sync::Lazy;
use ruma::{
    events::{
        room::message::{MessageType, RoomMessageEventContent},
        FullStateEventContent,
    },
    room_id,
};
use serde_json::{json, Value as JsonValue};
use stream_assert::{assert_next_eq, assert_next_matches};
use tokio::time::{sleep, timeout};
use wiremock::{
    matchers::{header, method, path_regex, query_param, query_param_is_missing},
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
        .respond_with(ResponseTemplate::new(200).set_body_json(&*ROOM_MESSAGES_BATCH_1))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    let paginate = async {
        timeline.paginate_backwards(PaginationOptions::simple_request(10)).await.unwrap();
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
    assert_let!(TimelineItemContent::Message(msg) = message.as_event().unwrap().content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "hello world");

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    assert_let!(TimelineItemContent::Message(msg) = message.as_event().unwrap().content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "the world is big");

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    assert_let!(TimelineItemContent::OtherState(state) = message.as_event().unwrap().content());
    assert_eq!(state.state_key(), "");
    assert_let!(
        AnyOtherFullStateEventContent::RoomName(FullStateEventContent::Original {
            content,
            prev_content
        }) = state.content()
    );
    assert_eq!(content.name, "New room name");
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

    timeline.paginate_backwards(PaginationOptions::simple_request(10)).await.unwrap();
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

    timeline.paginate_backwards(PaginationOptions::simple_request(10)).await.unwrap();
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

#[async_test]
async fn wait_for_token() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);

    let from = "t392-516_47314_0_7_1_1_1_11444_1";
    let mut back_pagination_status = timeline.back_pagination_status();

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", from)) // make sure the right token is sent
        .respond_with(ResponseTemplate::new(200).set_body_json(&*ROOM_MESSAGES_BATCH_1))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(event_builder.make_sync_message_event(
                &ALICE,
                RoomMessageEventContent::text_plain("live event!"),
            ))
            .set_timeline_prev_batch(from.to_owned()),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    let paginate = async {
        timeline
            .paginate_backwards(PaginationOptions::simple_request(10).wait_for_token())
            .await
            .unwrap();
    };
    let observe_paginating = async {
        assert_eq!(back_pagination_status.next().await, Some(BackPaginationStatus::Paginating));
        assert_eq!(back_pagination_status.next().await, Some(BackPaginationStatus::Idle));
    };
    let sync = async {
        // Make sure syncing starts a little bit later than pagination
        sleep(Duration::from_millis(100)).await;
        client.sync_once(sync_settings.clone()).await.unwrap();
    };
    timeout(Duration::from_secs(4), join3(paginate, observe_paginating, sync)).await.unwrap();

    // Make sure pagination was called (with the right parameters)
    server.verify().await;
}

#[async_test]
async fn dedup() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);

    let from = "t392-516_47314_0_7_1_1_1_11444_1";

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&*ROOM_MESSAGES_BATCH_1)
                // Set a small delay to make sure the response doesn't arrive
                // before both paginate_backwards function calls have happened.
                .set_delay(Duration::from_millis(100)),
        )
        .expect(1) // endpoint should only be called once
        .named("messages_batch_1")
        .mount(&server)
        .await;

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(event_builder.make_sync_message_event(
                &ALICE,
                RoomMessageEventContent::text_plain("live event!"),
            ))
            .set_timeline_prev_batch(from.to_owned()),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    let paginate_1 = async {
        timeline.paginate_backwards(PaginationOptions::simple_request(10)).await.unwrap();
    };
    let paginate_2 = async {
        timeline.paginate_backwards(PaginationOptions::simple_request(10)).await.unwrap();
    };
    timeout(Duration::from_secs(2), join(paginate_1, paginate_2)).await.unwrap();

    // Make sure pagination was called (with the right parameters)
    server.verify().await;
}

#[async_test]
async fn timeline_reset_while_paginating() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(event_builder.make_sync_message_event(
                &ALICE,
                RoomMessageEventContent::text_plain("live event!"),
            ))
            .set_timeline_prev_batch("pagination_1".to_owned()),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Next sync response will response with pagination_2 token and limited
    // response, resetting the timeline
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(event_builder.make_sync_message_event(
                &BOB,
                RoomMessageEventContent::text_plain("new live event."),
            ))
            .set_timeline_limited()
            .set_timeline_prev_batch("pagination_2".to_owned()),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    // pagination with first token
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "pagination_1"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "chunk": [],
                    "start": "pagination_1",
                    "end": "some_other_token",
                }))
                // Make sure the concurrent sync request returns first
                .set_delay(Duration::from_millis(200)),
        )
        .expect(1)
        .named("pagination_1")
        .mount(&server)
        .await;

    // pagination with second token
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "pagination_2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": [],
            "start": "pagination_2",
        })))
        .expect(1)
        .named("pagination_2")
        .mount(&server)
        .await;

    let mut back_pagination_status = timeline.back_pagination_status();

    let paginate = async {
        timeline
            .paginate_backwards(PaginationOptions::simple_request(10).wait_for_token())
            .await
            .unwrap();
    };
    let observe_paginating = async {
        assert_eq!(back_pagination_status.next().await, Some(BackPaginationStatus::Paginating));
        // timeline start reached because second pagination response contains
        // no end field
        assert_eq!(
            back_pagination_status.next().await,
            Some(BackPaginationStatus::TimelineStartReached)
        );
    };
    let sync = async {
        client.sync_once(sync_settings.clone()).await.unwrap();
    };
    timeout(Duration::from_secs(2), join3(paginate, observe_paginating, sync)).await.unwrap();

    // No events in back-pagination responses, day divider + event from latest
    // sync is present
    assert_eq!(timeline.items().await.len(), 2);

    // Make sure both pagination mocks were called
    server.verify().await;
}

pub static ROOM_MESSAGES_BATCH_1: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "chunk": [
          {
            "age": 1042,
            "content": {
              "body": "hello world",
              "msgtype": "m.text"
            },
            "event_id": "$1444812213350496Caaaf:example.com",
            "origin_server_ts": 1444812213737i64,
            "room_id": "!Xq3620DUiqCaoxq:example.com",
            "sender": "@alice:example.com",
            "type": "m.room.message"
          },
          {
            "age": 20123,
            "content": {
              "body": "the world is big",
              "msgtype": "m.text"
            },
            "event_id": "$1444812213350496Cbbbf:example.com",
            "origin_server_ts": 1444812194656i64,
            "room_id": "!Xq3620DUiqCaoxq:example.com",
            "sender": "@bob:example.com",
            "type": "m.room.message"
          },
          {
            "age": 50789,
            "content": {
              "name": "New room name"
            },
            "event_id": "$1444812213350496Ccccf:example.com",
            "origin_server_ts": 1444812163990i64,
            "unsigned": {
              "prev_content": {
                "name": "Old room name",
              },
            },
            "room_id": "!Xq3620DUiqCaoxq:example.com",
            "sender": "@bob:example.com",
            "state_key": "",
            "type": "m.room.name"
          }
        ],
        "end": "t47409-4357353_219380_26003_2269",
        "start": "t392-516_47314_0_7_1_1_1_11444_1"
    })
});

pub static ROOM_MESSAGES_BATCH_2: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "chunk": [
          {
            "age": 1042,
            "content": {
              "body": "hello room then",
              "msgtype": "m.text"
            },
            "event_id": "$1444812213350496Cdddf:example.com",
            "origin_server_ts": 1444812213737i64,
            "room_id": "!Xq3620DUiqCaoxq:example.com",
            "sender": "@alice:example.com",
            "type": "m.room.message"
          },
        ],
        "start": "t54392-516_47314_0_7_1_1_1_11444_1",
        "start": "t59392-516_47314_0_7_1_1_1_11444_1",
    })
});

#[async_test]
async fn empty_chunk() {
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

    // It should try to do another request after the empty chunk.
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(query_param_is_missing("from"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": [],
            "start": "t112-4357353_219380_26003_2269",
            "end": "t392-516_47314_0_7_1_1_1_11444_1",
        })))
        .expect(1)
        .named("messages_empty_chunk")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(query_param("from", "t392-516_47314_0_7_1_1_1_11444_1"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*ROOM_MESSAGES_BATCH_1))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    let paginate = async {
        timeline.paginate_backwards(PaginationOptions::simple_request(10)).await.unwrap();
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
    assert_let!(TimelineItemContent::Message(msg) = message.as_event().unwrap().content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "hello world");

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    assert_let!(TimelineItemContent::Message(msg) = message.as_event().unwrap().content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "the world is big");

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    assert_let!(TimelineItemContent::OtherState(state) = message.as_event().unwrap().content());
    assert_eq!(state.state_key(), "");
    assert_let!(
        AnyOtherFullStateEventContent::RoomName(FullStateEventContent::Original {
            content,
            prev_content
        }) = state.content()
    );
    assert_eq!(content.name, "New room name");
    assert_eq!(prev_content.as_ref().unwrap().name.as_ref().unwrap(), "Old room name");
}

#[async_test]
async fn until_num_items_with_empty_chunk() {
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
        .and(query_param_is_missing("from"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*ROOM_MESSAGES_BATCH_1))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(query_param("from", "t47409-4357353_219380_26003_2269"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": [],
            "start": "t47409-4357353_219380_26003_2269",
            "end": "t54392-516_47314_0_7_1_1_1_11444_1",
        })))
        .expect(1)
        .named("messages_empty_chunk")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(query_param("from", "t54392-516_47314_0_7_1_1_1_11444_1"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*ROOM_MESSAGES_BATCH_2))
        .expect(1)
        .named("messages_batch_2")
        .mount(&server)
        .await;

    let paginate = async {
        timeline.paginate_backwards(PaginationOptions::until_num_items(4, 4)).await.unwrap();
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
    assert_let!(TimelineItemContent::Message(msg) = message.as_event().unwrap().content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "hello world");

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    assert_let!(TimelineItemContent::Message(msg) = message.as_event().unwrap().content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "the world is big");

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    assert_let!(TimelineItemContent::OtherState(state) = message.as_event().unwrap().content());
    assert_eq!(state.state_key(), "");
    assert_let!(
        AnyOtherFullStateEventContent::RoomName(FullStateEventContent::Original {
            content,
            prev_content
        }) = state.content()
    );
    assert_eq!(content.name, "New room name");
    assert_eq!(prev_content.as_ref().unwrap().name.as_ref().unwrap(), "Old room name");

    let message = assert_next_matches!(
        timeline_stream,
        VectorDiff::Insert { index: 1, value } => value
    );
    assert_let!(TimelineItemContent::Message(msg) = message.as_event().unwrap().content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "hello room then");
}
