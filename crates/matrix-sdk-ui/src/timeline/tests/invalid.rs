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

use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk::deserialized_responses::TimelineEvent;
use matrix_sdk_test::{ALICE, BOB, async_test, sync_timeline_event};
use ruma::{
    MilliSecondsSinceUnixEpoch,
    events::{MessageLikeEventType, StateEventType, room::message::MessageType},
    uint,
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::TimelineItemContent;

#[async_test]
async fn test_invalid_edit() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("test").sender(&ALICE)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let msg = item.content().as_message().unwrap();
    assert_eq!(msg.body(), "test");

    let msg_event_id = item.event_id().unwrap();

    // Edit is from a different user than the previous event
    timeline
        .handle_live_event(
            f.text_msg(" * fake")
                .edit(msg_event_id, MessageType::text_plain("fake").into())
                .sender(&BOB),
        )
        .await;

    // Can't easily test the non-arrival of an item using the stream. Instead
    // just assert that there is still just a couple items in the timeline.
    assert_eq!(timeline.controller.items().await.len(), 2);
}

#[async_test]
async fn test_invalid_event_content() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    // m.room.message events must have a msgtype and body in content, so this
    // event with an empty content object should fail to deserialize.
    timeline
        .handle_live_event(TimelineEvent::from_plaintext(sync_timeline_event!({
            "content": {},
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 10,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        })))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.sender(), "@alice:example.org");
    assert_eq!(item.event_id().unwrap(), "$eeG0HA0FAZ37wP8kXlNkxx3I");
    assert_eq!(item.timestamp(), MilliSecondsSinceUnixEpoch(uint!(10)));
    assert_let!(TimelineItemContent::FailedToParseMessageLike { event_type, .. } = item.content());
    assert_eq!(*event_type, MessageLikeEventType::RoomMessage);

    // Similar to above, the m.room.member state event must also not have an
    // empty content object.
    timeline
        .handle_live_event(TimelineEvent::from_plaintext(sync_timeline_event!({
            "content": {},
            "event_id": "$d5G0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 2179,
            "sender": "@alice:example.org",
            "type": "m.room.member",
            "state_key": "@alice:example.org",
        })))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.sender(), "@alice:example.org");
    assert_eq!(item.event_id().unwrap(), "$d5G0HA0FAZ37wP8kXlNkxx3I");
    assert_eq!(item.timestamp(), MilliSecondsSinceUnixEpoch(uint!(2179)));
    assert_let!(
        TimelineItemContent::FailedToParseState { event_type, state_key, .. } = item.content()
    );
    assert_eq!(*event_type, StateEventType::RoomMember);
    assert_eq!(state_key, "@alice:example.org");
}

#[async_test]
async fn test_invalid_event() {
    let timeline = TestTimeline::new();

    // This event is missing the sender field which the homeserver must add to
    // all timeline events. Because the event is malformed, it will be ignored.
    timeline
        .handle_live_event(TimelineEvent::from_plaintext(sync_timeline_event!({
            "content": {
                "body": "hello world",
                "msgtype": "m.text"
            },
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 10,
            "type": "m.room.message",
        })))
        .await;
    assert_eq!(timeline.controller.items().await.len(), 0);
}
