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

use std::{ops::Not, sync::Arc, time::Duration};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::{
    future::{join, join3},
    FutureExt, StreamExt as _,
};
use matrix_sdk::{
    config::SyncSettings,
    event_cache::RoomPaginationStatus,
    test_utils::{
        logged_in_client_with_server,
        mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    },
};
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, mocks::mock_encryption_state, JoinedRoomBuilder,
    StateTestEvent, SyncResponseBuilder, ALICE, BOB,
};
use matrix_sdk_ui::timeline::{AnyOtherFullStateEventContent, RoomExt, TimelineItemContent};
use once_cell::sync::Lazy;
use ruma::{
    events::{room::message::MessageType, FullStateEventContent},
    room_id, EventId,
};
use serde_json::{json, Value as JsonValue};
use stream_assert::{assert_next_eq, assert_pending};
use tokio::{
    spawn,
    time::{sleep, timeout},
};
use wiremock::{
    matchers::{header, method, path_regex, query_param, query_param_is_missing},
    Mock, ResponseTemplate,
};

use crate::{mock_sync, timeline::sliding_sync::assert_timeline_stream};

#[async_test]
async fn test_back_pagination() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) = timeline.subscribe().await;
    let (_, mut back_pagination_status) = timeline.live_back_pagination_status().await.unwrap();

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*ROOM_MESSAGES_BATCH_1))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    let paginate = async {
        timeline.paginate_backwards(10).await.unwrap();
        server.reset().await;
    };
    let observe_paginating = async {
        assert_eq!(back_pagination_status.next().await, Some(RoomPaginationStatus::Paginating));
    };
    join(paginate, observe_paginating).await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);

    // `m.room.name`
    {
        assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
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

    // `m.room.name` receives an update
    {
        assert_let!(VectorDiff::Set { index, .. } = &timeline_updates[1]);
        assert_eq!(*index, 0);
    }

    // `m.room.message`: “the world is big”
    {
        assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[2]);
        assert_let!(Some(msg) = message.as_event().unwrap().content().as_message());
        assert_let!(MessageType::Text(text) = msg.msgtype());
        assert_eq!(text.body, "the world is big");
    }

    // `m.room.message`: “hello world”
    {
        assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[3]);
        assert_let!(Some(msg) = message.as_event().unwrap().content().as_message());
        assert_let!(MessageType::Text(text) = msg.msgtype());
        assert_eq!(text.body, "hello world");
    }

    // Date divider is updated.
    {
        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[4]);
        assert!(date_divider.is_date_divider());
    }

    assert_pending!(timeline_stream);

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

    let hit_start = timeline.paginate_backwards(10).await.unwrap();
    assert!(hit_start);
    assert_next_eq!(
        back_pagination_status,
        RoomPaginationStatus::Idle { hit_timeline_start: true }
    );

    // Timeline start is inserted.
    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 1);
        assert_let!(VectorDiff::PushFront { value } = &timeline_updates[0]);
        assert!(value.is_timeline_start());
    }

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_back_pagination_highlighted() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder
        // We need the member event and power levels locally so the push rules processor works.
        .add_joined_room(
            JoinedRoomBuilder::new(room_id)
                .add_state_event(StateTestEvent::Member)
                .add_state_event(StateTestEvent::PowerLevels),
        );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
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

    timeline.paginate_backwards(10).await.unwrap();
    server.reset().await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);

    // `m.room.tombstone`
    {
        assert_let!(VectorDiff::PushBack { value: second } = &timeline_updates[0]);
        let remote_event = second.as_event().unwrap();
        // `m.room.tombstone` should be highlighted by default.
        assert!(remote_event.is_highlighted());
    }

    // `m.room.message`
    {
        assert_let!(VectorDiff::PushBack { value: first } = &timeline_updates[1]);
        let remote_event = first.as_event().unwrap();
        // Own events don't trigger push rules.
        assert!(!remote_event.is_highlighted());
    }

    // Date divider
    {
        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[2]);
        assert!(date_divider.is_date_divider());
    }

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_wait_for_token() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;

    client.event_cache().subscribe().unwrap();
    client.event_cache().enable_storage().unwrap();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let f = EventFactory::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());

    let from = "t392-516_47314_0_7_1_1_1_11444_1";
    let (_, mut back_pagination_status) = timeline.live_back_pagination_status().await.unwrap();

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
            .add_timeline_event(f.text_msg("live event!").sender(&ALICE))
            .set_timeline_prev_batch(from.to_owned())
            .set_timeline_limited(),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    let paginate = async {
        timeline.paginate_backwards(10).await.unwrap();
    };
    let observe_paginating = async {
        assert_eq!(back_pagination_status.next().await, Some(RoomPaginationStatus::Paginating));
        assert_eq!(
            back_pagination_status.next().await,
            Some(RoomPaginationStatus::Idle { hit_timeline_start: false })
        );
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
async fn test_dedup_pagination() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let f = EventFactory::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());

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
            .add_timeline_event(f.text_msg("live event!").sender(&ALICE))
            .set_timeline_prev_batch(from.to_owned()),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    // If I try to paginate twice at the same time,
    let paginate_1 = async {
        timeline.paginate_backwards(10).await.unwrap();
    };
    let paginate_2 = async {
        timeline.paginate_backwards(10).await.unwrap();
    };
    timeout(Duration::from_secs(5), join(paginate_1, paginate_2)).await.unwrap();

    // Then only one request is actually sent to the server (i.e. the number of
    // `expect()`ed requested is indeed 1.
    //
    // Make sure pagination was called (with the right parameters).
    server.verify().await;
}

#[async_test]
async fn test_timeline_reset_while_paginating() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let f = EventFactory::new();
    let mut sync_builder = SyncResponseBuilder::new();

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("live event!").sender(&ALICE))
            .set_timeline_prev_batch("pagination_1".to_owned())
            .set_timeline_limited(),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Next sync response will response with pagination_2 token and limited
    // response, resetting the timeline
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("new live event.").sender(&BOB))
            .set_timeline_prev_batch("pagination_2".to_owned())
            .set_timeline_limited(),
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

    let (_, mut back_pagination_status) = timeline.live_back_pagination_status().await.unwrap();

    let paginate = async { timeline.paginate_backwards(10).await.unwrap() };

    let observe_paginating = async {
        let mut seen_paginating = false;

        // Observe paginating updates: we want to make sure we see at least once
        // Paginating, and that it settles with Idle.
        while let Ok(update) =
            timeout(Duration::from_millis(500), back_pagination_status.next()).await
        {
            match update {
                Some(state) => {
                    if state == RoomPaginationStatus::Paginating {
                        seen_paginating = true;
                    }
                }
                None => break,
            }
        }

        assert!(seen_paginating);

        let (status, _) = timeline.live_back_pagination_status().await.unwrap();

        // Timeline start reached because second pagination response contains no end
        // field.
        assert_eq!(status, RoomPaginationStatus::Idle { hit_timeline_start: true });
    };

    let sync = async {
        client.sync_once(sync_settings.clone()).await.unwrap();
    };

    let (hit_start, _, _) =
        timeout(Duration::from_secs(5), join3(paginate, observe_paginating, sync)).await.unwrap();

    // Timeline start reached because second pagination response contains no end
    // field.
    assert!(hit_start);

    // No events in back-pagination responses, start of timeline + date divider +
    // event from latest sync is present
    assert_eq!(timeline.items().await.len(), 3);

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
async fn test_empty_chunk() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) = timeline.subscribe().await;
    let (_, mut back_pagination_status) = timeline.live_back_pagination_status().await.unwrap();

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
        timeline.paginate_backwards(10).await.unwrap();
        server.reset().await;
    };
    let observe_paginating = async {
        assert_eq!(back_pagination_status.next().await, Some(RoomPaginationStatus::Paginating));
    };
    join(paginate, observe_paginating).await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);

    // `m.room.name`
    {
        assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
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

    // `m.room.name` is updated
    {
        assert_let!(VectorDiff::Set { index, .. } = &timeline_updates[1]);
        assert_eq!(*index, 0);
    }

    // `m.room.message`: “the world is big”
    {
        assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[2]);
        assert_let!(Some(msg) = message.as_event().unwrap().content().as_message());
        assert_let!(MessageType::Text(text) = msg.msgtype());
        assert_eq!(text.body, "the world is big");
    }

    // `m.room.name`: “hello world”
    {
        assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[3]);
        assert_let!(Some(msg) = message.as_event().unwrap().content().as_message());
        assert_let!(MessageType::Text(text) = msg.msgtype());
        assert_eq!(text.body, "hello world");
    }

    // Date divider
    {
        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[4]);
        assert!(date_divider.is_date_divider());
    }

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_until_num_items_with_empty_chunk() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut timeline_stream) = timeline.subscribe().await;
    let (_, mut back_pagination_status) = timeline.live_back_pagination_status().await.unwrap();

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
        timeline.paginate_backwards(10).await.unwrap();
    };
    let observe_paginating = async {
        assert_eq!(back_pagination_status.next().await, Some(RoomPaginationStatus::Paginating));
    };
    join(paginate, observe_paginating).await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);

    // `m.room.name`
    {
        assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
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

    // `m.room.name` is updated
    {
        assert_let!(VectorDiff::Set { index, .. } = &timeline_updates[1]);
        assert_eq!(*index, 0);
    }

    // `m.room.message`: “the world is big”
    {
        assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[2]);
        assert_let!(Some(msg) = message.as_event().unwrap().content().as_message());
        assert_let!(MessageType::Text(text) = msg.msgtype());
        assert_eq!(text.body, "the world is big");
    }

    // `m.room.name`: “hello world”
    {
        assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[3]);
        assert_let!(Some(msg) = message.as_event().unwrap().content().as_message());
        assert_let!(MessageType::Text(text) = msg.msgtype());
        assert_eq!(text.body, "hello world");
    }

    // Date divider
    {
        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[4]);
        assert!(date_divider.is_date_divider());
    }

    let reached_start = timeline.paginate_backwards(10).await.unwrap();
    assert!(reached_start);

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);
    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[0]);
    assert!(value.is_timeline_start());

    // `m.room.name`: “hello room then”
    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Insert { index: 2, value: message } = &timeline_updates[0]);
        assert_let!(Some(msg) = message.as_event().unwrap().content().as_message());
        assert_let!(MessageType::Text(text) = msg.msgtype());
        assert_eq!(text.body, "hello room then");
    }

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_back_pagination_aborted() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());
    let (_, mut back_pagination_status) = timeline.live_back_pagination_status().await.unwrap();

    // Delay the server response, so we have time to abort the request.
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(&*ROOM_MESSAGES_BATCH_1)
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&server)
        .await;

    let paginate = spawn({
        let timeline = timeline.clone();
        async move {
            timeline.paginate_backwards(10).await.unwrap();
        }
    });

    assert_eq!(back_pagination_status.next().await, Some(RoomPaginationStatus::Paginating));

    // Abort the pagination!
    paginate.abort();

    // The task should finish with a cancellation.
    assert!(paginate.await.unwrap_err().is_cancelled());

    // The timeline should automatically reset to idle.
    assert_next_eq!(
        back_pagination_status,
        RoomPaginationStatus::Idle { hit_timeline_start: false }
    );

    // And there should be no other pending pagination status updates.
    assert!(back_pagination_status.next().now_or_never().is_none());
}

#[async_test]
async fn test_lazy_back_pagination() {
    let room_id = room_id!("!foo:bar.baz");
    let event_factory = EventFactory::new().room(room_id).sender(&ALICE);

    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    let room = mock_server.sync_joined_room(&client, room_id).await;
    let timeline = room.timeline().await.unwrap();
    let (timeline_initial_items, mut timeline_stream) = timeline.subscribe().await;

    assert!(timeline_initial_items.is_empty());
    assert_pending!(timeline_stream);

    // Receive 30 events.
    mock_server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_joined_room({
                let mut room = JoinedRoomBuilder::new(room_id);

                for nth in 0..30 {
                    room = room.add_timeline_event(
                        event_factory
                            .text_msg("foo")
                            .event_id(&EventId::parse(format!("$ev{nth}")).unwrap()),
                    );
                }

                room.set_timeline_prev_batch("back_pagination_token_1").set_timeline_limited()
            });
        })
        .await;

    // Only 20 items are broadcasted to the subscriber.
    assert_timeline_stream! {
        [timeline_stream]
        clear;

        // `$ev11`, the 1st item.
        append "$ev11";

        // `$ev12` and the update of `$ev11` (because of read receipt)
        update[0] "$ev11";
        append "$ev12";

        // and so on…
        update[1] "$ev12";
        append "$ev13";

        update[2] "$ev13";
        append "$ev14";

        update[3] "$ev14";
        append "$ev15";

        update[4] "$ev15";
        append "$ev16";

        update[5] "$ev16";
        append "$ev17";

        update[6] "$ev17";
        append "$ev18";

        update[7] "$ev18";
        append "$ev19";

        update[8] "$ev19";
        append "$ev20";

        update[9] "$ev20";
        append "$ev21";

        update[10] "$ev21";
        append "$ev22";

        update[11] "$ev22";
        append "$ev23";

        update[12] "$ev23";
        append "$ev24";

        update[13] "$ev24";
        append "$ev25";

        update[14] "$ev25";
        append "$ev26";

        update[15] "$ev26";
        append "$ev27";

        update[16] "$ev27";
        append "$ev28";

        update[17] "$ev28";
        // … until we receive `$ev29`: the last received event, and the 19th item.
        append "$ev29";

        // The day divider is inserted before `$ev0`, so it shifts items to
        // the right: `$ev10` becomes part of the stream.
        //
        // This is the 20th item, hurray!
        prepend "$ev10";
    };

    // Nothing more!
    assert_pending!(timeline_stream);

    // Now let's do a backwards pagination of 5 items.
    {
        let hit_end_of_timeline = timeline.paginate_backwards(5).await.unwrap();

        assert!(hit_end_of_timeline.not());

        // Oh, 5 new items, without even hitting the network because the timeline
        // already has these!
        assert_timeline_stream! {
            [timeline_stream]
            prepend "$ev9";
            prepend "$ev8";
            prepend "$ev7";
            prepend "$ev6";
            prepend "$ev5";
        };
        assert_pending!(timeline_stream);
    }

    // Let's do another backwards pagination of 3 items.
    {
        let hit_end_of_timeline = timeline.paginate_backwards(3).await.unwrap();

        assert!(hit_end_of_timeline.not());

        // Boooring, 3 new items, without even hitting the network, yet again.
        assert_timeline_stream! {
            [timeline_stream]
            prepend "$ev4";
            prepend "$ev3";
            prepend "$ev2";
        };
        assert_pending!(timeline_stream);
    }

    // This time, let's run another backwards pagination of 6 items. 2 items are
    // from in-memory events, 1 item is a virtual item, and 3 will be created from
    // events fetched from the network.
    {
        let network_pagination = mock_server
            .mock_room_messages()
            .match_from("back_pagination_token_1")
            .ok(RoomMessagesResponseTemplate::default()
                .end_token("back_pagination_token_2")
                .events({
                    (100..103)
                        .map(|nth| {
                            event_factory
                                .text_msg("foo")
                                .event_id(&EventId::parse(format!("$ev{nth}")).unwrap())
                        })
                        .collect::<Vec<_>>()
                }))
            .mock_once()
            .mount_as_scoped()
            .await;

        let hit_end_of_timeline = timeline.paginate_backwards(6).await.unwrap();

        assert!(hit_end_of_timeline.not());

        // Boooring, 3 new items from the in-memory timeline items.
        assert_timeline_stream! {
            [timeline_stream]
            prepend "$ev1";
            prepend "$ev0";
            prepend --- date divider ---;
        };
        // And 3 other items representing the events received from the network backwards
        // pagination.
        //
        // They are inserted after the date divider, hence the indices 1, 2 and 3.
        assert_timeline_stream! {
            [timeline_stream]
            insert[1] "$ev102";
            insert[2] "$ev101";
            insert[3] "$ev100";
        };
        // Oh yeah B-).
        assert_pending!(timeline_stream);

        drop(network_pagination);
    }

    // Finally, let's run a last backwards pagination of 5 items, fully hitting the
    // network, but 2 will be returned because the beginning of the timeline is
    // reached.
    {
        let network_pagination = mock_server
            .mock_room_messages()
            .match_from("back_pagination_token_2")
            .ok(RoomMessagesResponseTemplate::default()
                // No previous batch token, the beginning of the timeline is reached.
                .events({
                    (200..202)
                        .map(|nth| {
                            event_factory
                                .text_msg("foo")
                                .event_id(&EventId::parse(format!("$ev{nth}")).unwrap())
                        })
                        .collect::<Vec<_>>()
                }))
            .mock_once()
            .mount_as_scoped()
            .await;

        let hit_end_of_timeline = timeline.paginate_backwards(5).await.unwrap();

        assert!(hit_end_of_timeline);

        // The start of the timeline is inserted as its own timeline update.
        assert_timeline_stream! {
            [timeline_stream]
            prepend --- timeline start ---;
        };

        // Receive 3 new items.
        //
        // They are inserted after the date divider and start of timeline, hence the
        // indices 2 and 3.

        assert_timeline_stream! {
            [timeline_stream]
            insert[2] "$ev201";
            insert[3] "$ev200";
        };
        // So cool.
        assert_pending!(timeline_stream);

        drop(network_pagination);
    }
}
