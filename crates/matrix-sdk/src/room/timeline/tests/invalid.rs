use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk_test::async_test;
use ruma::{
    assign, event_id,
    events::{
        relation::Replacement,
        room::message::{self, MessageType, RoomMessageEventContent},
        MessageLikeEventType, StateEventType,
    },
    uint, MilliSecondsSinceUnixEpoch,
};
use serde_json::json;

use super::{TestTimeline, ALICE, BOB};
use crate::room::timeline::TimelineItemContent;

#[async_test]
async fn invalid_edit() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("test")).await;
    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    let msg = event.content().as_message().unwrap();
    assert_eq!(msg.body(), "test");

    let msg_event_id = event.event_id();

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
    let mut stream = timeline.subscribe().await;

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

    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let event_item = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event_item.sender(), "@alice:example.org");
    assert_eq!(event_item.event_id(), event_id!("$eeG0HA0FAZ37wP8kXlNkxx3I").to_owned());
    assert_eq!(event_item.timestamp(), MilliSecondsSinceUnixEpoch(uint!(10)));
    let event_type = assert_matches!(
        event_item.content(),
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

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let event_item = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event_item.sender(), "@alice:example.org");
    assert_eq!(event_item.event_id(), event_id!("$d5G0HA0FAZ37wP8kXlNkxx3I").to_owned());
    assert_eq!(event_item.timestamp(), MilliSecondsSinceUnixEpoch(uint!(2179)));
    let (event_type, state_key) = assert_matches!(
        event_item.content(),
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
