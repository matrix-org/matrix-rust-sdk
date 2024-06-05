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
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{
    config::SyncSettings,
    test_utils::{events::EventFactory, logged_in_client_with_server},
};
use matrix_sdk_test::{
    async_test, sync_timeline_event, JoinedRoomBuilder, RoomAccountDataTestEvent, StateTestEvent,
    SyncResponseBuilder,
};
use matrix_sdk_ui::timeline::{EventSendState, RoomExt, TimelineItemContent, VirtualTimelineItem};
use ruma::{
    events::room::message::RoomMessageEventContent, room_id, user_id, MilliSecondsSinceUnixEpoch,
};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::mock_sync;

mod echo;
mod edit;
mod focus_event;
mod pagination;
mod profiles;
mod queue;
mod read_receipts;
mod replies;
mod subscribe;

pub(crate) mod sliding_sync;

#[async_test]
async fn test_reaction() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": "$TTvQUp1e17qkw41rBSjpZ",
                "origin_server_ts": 152037280,
                "sender": "@alice:example.org",
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "m.relates_to": {
                        "event_id": "$TTvQUp1e17qkw41rBSjpZ",
                        "key": "👍",
                        "rel_type": "m.annotation",
                    },
                },
                "event_id": "$031IXQRi27504",
                "origin_server_ts": 152038300,
                "sender": "@bob:example.org",
                "type": "m.reaction",
            })),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // The new message starts with their author's read receipt.
    assert_let!(Some(VectorDiff::PushBack { value: message }) = timeline_stream.next().await);
    let event_item = message.as_event().unwrap();
    assert_matches!(event_item.content(), TimelineItemContent::Message(_));
    assert_eq!(event_item.read_receipts().len(), 1);

    // The new message is getting the reaction, which implies an implicit read
    // receipt that's obtained first.
    assert_let!(
        Some(VectorDiff::Set { index: 0, value: updated_message }) = timeline_stream.next().await
    );
    let event_item = updated_message.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(msg) = event_item.content());
    assert!(!msg.is_edited());
    assert_eq!(event_item.read_receipts().len(), 2);
    assert_eq!(event_item.reactions().len(), 0);

    // Then the reaction is taken into account.
    assert_let!(
        Some(VectorDiff::Set { index: 0, value: updated_message }) = timeline_stream.next().await
    );
    let event_item = updated_message.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(msg) = event_item.content());
    assert!(!msg.is_edited());
    assert_eq!(event_item.read_receipts().len(), 2);
    assert_eq!(event_item.reactions().len(), 1);
    let group = &event_item.reactions()["👍"];
    assert_eq!(group.len(), 1);
    let senders: Vec<_> = group.senders().map(|v| &v.sender_id).collect();
    assert_eq!(senders.as_slice(), [user_id!("@bob:example.org")]);

    // The day divider.
    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {},
            "redacts": "$031IXQRi27504",
            "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
            "sender": "@bob:example.org",
            "origin_server_ts": 152037280,
            "type": "m.room.redaction",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(
        Some(VectorDiff::Set { index: 1, value: updated_message }) = timeline_stream.next().await
    );
    let event_item = updated_message.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(msg) = event_item.content());
    assert!(!msg.is_edited());
    assert_eq!(event_item.reactions().len(), 0);
}

#[async_test]
async fn test_redacted_message() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {},
                "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
                "origin_server_ts": 152035910,
                "sender": "@alice:example.org",
                "type": "m.room.message",
                "unsigned": {
                    "redacted_because": {
                        "content": {},
                        "redacts": "$eeG0HA0FAZ37wP8kXlNkxx3I",
                        "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
                        "sender": "@alice:example.org",
                        "origin_server_ts": 152037280,
                        "type": "m.room.redaction",
                    },
                },
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {},
                "redacts": "$eeG0HA0FAZ37wP8kXlNkxx3I",
                "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
                "sender": "@alice:example.org",
                "origin_server_ts": 152037280,
                "type": "m.room.redaction",
            })),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    assert_matches!(first.as_event().unwrap().content(), TimelineItemContent::RedactedMessage);

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());
}

#[async_test]
async fn test_redact_message() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let factory = EventFactory::new();
    factory.set_next_ts(MilliSecondsSinceUnixEpoch::now().get().into());

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(
            factory.sender(user_id!("@a:b.com")).text_msg("buy my bitcoins bro"),
        ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    assert_eq!(
        first.as_event().unwrap().content().as_message().unwrap().body(),
        "buy my bitcoins bro"
    );

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    // Redacting a remote event works.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/redact/.*?/.*?"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "event_id": "$42"
        })))
        .mount(&server)
        .await;

    let event_id = first.as_event().unwrap();

    let did_redact = timeline.redact(event_id, Some("inapprops")).await.unwrap();
    assert!(did_redact);

    // Redacting a local event works.
    timeline
        .send(RoomMessageEventContent::text_plain("i will disappear soon").into())
        .await
        .unwrap();

    assert_let!(Some(VectorDiff::PushBack { value: second }) = timeline_stream.next().await);

    let second = second.as_event().unwrap();
    assert_matches!(second.send_state(), Some(EventSendState::NotSentYet));

    // We haven't set a route for sending events, so this will fail.
    assert_let!(Some(VectorDiff::Set { index, value: second }) = timeline_stream.next().await);
    assert_eq!(index, 2);

    let second = second.as_event().unwrap();
    assert!(second.is_local_echo());
    assert_matches!(second.send_state(), Some(EventSendState::SendingFailed { .. }));

    // Let's redact the local echo.
    let did_redact = timeline.redact(second, None).await.unwrap();
    assert!(did_redact);

    // Observe local echo being removed.
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 2 }));
}

#[async_test]
async fn test_read_marker() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "body": "hello",
                "msgtype": "m.text",
            },
            "event_id": "$someplace:example.org",
            "origin_server_ts": 152037280,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: message }) = timeline_stream.next().await);
    assert_matches!(message.as_event().unwrap().content(), TimelineItemContent::Message(_));

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_account_data(RoomAccountDataTestEvent::FullyRead),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Nothing should happen, the marker cannot be added at the end.

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "body": "hello to you!",
                "msgtype": "m.text",
            },
            "event_id": "$someotherplace:example.org",
            "origin_server_ts": 152067280,
            "sender": "@bob:example.org",
            "type": "m.room.message",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: message }) = timeline_stream.next().await);
    assert_matches!(message.as_event().unwrap().content(), TimelineItemContent::Message(_));

    assert_let!(
        Some(VectorDiff::Insert { index: 2, value: marker }) = timeline_stream.next().await
    );
    assert_matches!(marker.as_virtual().unwrap(), VirtualTimelineItem::ReadMarker);
}

#[async_test]
async fn test_sync_highlighted() {
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

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "body": "hello",
                "msgtype": "m.text",
            },
            "event_id": "$msda7m0df9E9op3",
            "origin_server_ts": 152037280,
            "sender": "@example:localhost",
            "type": "m.room.message",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    let remote_event = first.as_event().unwrap();
    // Own events don't trigger push rules.
    assert!(!remote_event.is_highlighted());

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "body": "This room has been replaced",
                "replacement_room": "!newroom:localhost",
            },
            "event_id": "$foun39djjod0f",
            "origin_server_ts": 152039280,
            "sender": "@bob:localhost",
            "state_key": "",
            "type": "m.room.tombstone",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: second }) = timeline_stream.next().await);
    let remote_event = second.as_event().unwrap();
    // `m.room.tombstone` should be highlighted by default.
    assert!(remote_event.is_highlighted());
}
