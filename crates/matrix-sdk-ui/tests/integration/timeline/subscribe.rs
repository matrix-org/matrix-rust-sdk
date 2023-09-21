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
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{
    async_test, sync_timeline_event, GlobalAccountDataTestEvent, JoinedRoomBuilder,
    SyncResponseBuilder, TimelineTestEvent,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineItemContent, VirtualTimelineItem};
use ruma::{event_id, events::room::message::MessageType, room_id};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};

use crate::{logged_in_client, mock_sync};

#[async_test]
async fn batched() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().event_filter(|_| true).build().await;
    let (_, mut timeline_stream) = timeline.subscribe_batched().await;

    let hdl = tokio::spawn(async move {
        let next_batch = timeline_stream.next().await.unwrap();
        // One day divider, three event items
        assert_eq!(next_batch.len(), 4);
    });

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(TimelineTestEvent::MessageText)
            .add_timeline_event(TimelineTestEvent::Member)
            .add_timeline_event(TimelineTestEvent::MessageNotice),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    tokio::time::timeout(Duration::from_millis(500), hdl).await.unwrap().unwrap();
}

#[async_test]
async fn event_filter() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().event_filter(|_| true).build().await;
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let first_event_id = event_id!("$YTQwYl2ply");
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "body": "hello",
                "msgtype": "m.text",
            },
            "event_id": first_event_id,
            "origin_server_ts": 152037280,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        }),
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
    let first_event = first.as_event().unwrap();
    assert_eq!(first_event.event_id(), Some(first_event_id));
    let msg = assert_matches!(
        first_event.content(),
        TimelineItemContent::Message(msg) => msg
    );
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert!(!msg.is_edited());

    let second_event_id = event_id!("$Ga6Y2l0gKY");
    let edit_event_id = event_id!("$7i9In0gEmB");
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "Test",
                    "formatted_body": "<em>Test</em>",
                    "msgtype": "m.text",
                    "format": "org.matrix.custom.html",
                },
                "event_id": second_event_id,
                "origin_server_ts": 152038280,
                "sender": "@bob:example.org",
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": " * hi",
                    "m.new_content": {
                        "body": "hi",
                        "msgtype": "m.text",
                    },
                    "m.relates_to": {
                        "event_id": first_event_id,
                        "rel_type": "m.replace",
                    },
                    "msgtype": "m.text",
                },
                "event_id": edit_event_id,
                "origin_server_ts": 159056300,
                "sender": "@alice:example.org",
                "type": "m.room.message",
            })),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let second = assert_matches!(timeline_stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let second_event = second.as_event().unwrap();
    assert_eq!(second_event.event_id(), Some(second_event_id));

    // The edit is applied to the first event.
    let first = assert_matches!(
        timeline_stream.next().await,
        Some(VectorDiff::Set { index: 1, value }) => value
    );
    let first_event = first.as_event().unwrap();
    assert!(!first_event.read_receipts().is_empty());
    let msg = assert_matches!(
        first_event.content(),
        TimelineItemContent::Message(msg) => msg
    );
    let text = assert_matches!(msg.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "hi");
    assert!(msg.is_edited());
}

#[async_test]
async fn timeline_is_reset_when_a_user_is_ignored_or_unignored() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().build().await;
    let (_, timeline_stream) = timeline.subscribe().await;
    pin_mut!(timeline_stream);

    let alice = "@alice:example.org";
    let bob = "@bob:example.org";

    let first_event_id = event_id!("$YTQwYl2pl1");
    let second_event_id = event_id!("$YTQwYl2pl2");
    let third_event_id = event_id!("$YTQwYl2pl3");

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": first_event_id,
                "origin_server_ts": 152037280,
                "sender": alice,
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": second_event_id,
                "origin_server_ts": 152037281,
                "sender": bob,
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": third_event_id,
                "origin_server_ts": 152037282,
                "sender": alice,
                "type": "m.room.message",
            })),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.as_virtual(), Some(VirtualTimelineItem::DayDivider(_)));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(first_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(second_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(first_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(third_event_id));
    });
    assert_pending!(timeline_stream);

    let fourth_event_id = event_id!("$YTQwYl2pl4");
    let fiveth_event_id = event_id!("$YTQwYl2pl5");

    ev_builder.add_global_account_data_event(GlobalAccountDataTestEvent::Custom(json!({
        "content": {
            "ignored_users": {
                bob: {}
            }
        },
        "type": "m.ignored_user_list",
    })));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // The timeline has been emptied.
    assert_next_matches!(timeline_stream, VectorDiff::Clear);
    assert_pending!(timeline_stream);

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": fourth_event_id,
                "origin_server_ts": 152037283,
                "sender": alice,
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": fiveth_event_id,
                "origin_server_ts": 152037284,
                "sender": alice,
                "type": "m.room.message",
            })),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Timeline receives events as before.
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_matches!(value.as_virtual(), Some(VirtualTimelineItem::DayDivider(_)));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(fourth_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(fourth_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(fiveth_event_id));
    });
    assert_pending!(timeline_stream);
}
