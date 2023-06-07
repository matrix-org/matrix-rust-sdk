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

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use matrix_sdk_test::async_test;
use ruma::{
    assign,
    events::{
        relation::Replacement,
        room::message::{self, MessageType, RoomMessageEventContent},
        MessageLikeEventType, StateEventType,
    },
    uint, MilliSecondsSinceUnixEpoch,
};
use serde_json::json;
use stream_assert::assert_next_matches;

use super::{TestTimeline, ALICE, BOB};
use crate::timeline::TimelineItemContent;

#[async_test]
async fn invalid_edit() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("test")).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let msg = item.content().as_message().unwrap();
    assert_eq!(msg.body(), "test");

    let msg_event_id = item.event_id().unwrap();

    let edit = assign!(RoomMessageEventContent::text_plain(" * fake"), {
        relates_to: Some(message::Relation::Replacement(Replacement::new(
            msg_event_id.to_owned(),
            MessageType::text_plain("fake"),
        ))),
    });
    // Edit is from a different user than the previous event
    timeline.handle_live_message_event(&BOB, edit).await;

    // Can't easily test the non-arrival of an item using the stream. Instead
    // just assert that there is still just a couple items in the timeline.
    assert_eq!(timeline.inner.items().await.len(), 2);
}

#[async_test]
async fn invalid_event_content() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    // m.room.message events must have a msgtype and body in content, so this
    // event with an empty content object should fail to deserialize.
    timeline
        .handle_live_custom_event(json!({
            "content": {},
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 10,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        }))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.sender(), "@alice:example.org");
    assert_eq!(item.event_id().unwrap(), "$eeG0HA0FAZ37wP8kXlNkxx3I");
    assert_eq!(item.timestamp(), MilliSecondsSinceUnixEpoch(uint!(10)));
    let event_type = assert_matches!(
        item.content(),
        TimelineItemContent::FailedToParseMessageLike { event_type, .. } => event_type
    );
    assert_eq!(*event_type, MessageLikeEventType::RoomMessage);

    // Similar to above, the m.room.member state event must also not have an
    // empty content object.
    timeline
        .handle_live_custom_event(json!({
            "content": {},
            "event_id": "$d5G0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 2179,
            "sender": "@alice:example.org",
            "type": "m.room.member",
            "state_key": "@alice:example.org",
        }))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.sender(), "@alice:example.org");
    assert_eq!(item.event_id().unwrap(), "$d5G0HA0FAZ37wP8kXlNkxx3I");
    assert_eq!(item.timestamp(), MilliSecondsSinceUnixEpoch(uint!(2179)));
    let (event_type, state_key) = assert_matches!(
        item.content(),
        TimelineItemContent::FailedToParseState {
            event_type,
            state_key,
            ..
        } => (event_type, state_key)
    );
    assert_eq!(*event_type, StateEventType::RoomMember);
    assert_eq!(state_key, "@alice:example.org");
}

#[async_test]
async fn invalid_event() {
    let timeline = TestTimeline::new();

    // This event is missing the sender field which the homeserver must add to
    // all timeline events. Because the event is malformed, it will be ignored.
    timeline
        .handle_live_custom_event(json!({
            "content": {
                "body": "hello world",
                "msgtype": "m.text"
            },
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 10,
            "type": "m.room.message",
        }))
        .await;
    assert_eq!(timeline.inner.items().await.len(), 0);
}
