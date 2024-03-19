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
use matrix_sdk_test::{async_test, sync_timeline_event, ALICE};
use ruma::{
    assign,
    events::{
        relation::Replacement,
        room::message::{
            self, MessageType, RedactedRoomMessageEventContent, RoomMessageEventContent,
        },
    },
    server_name, EventId,
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::TimelineItemContent;

#[async_test]
async fn test_live_redacted() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_redacted_message_event(*ALICE, RedactedRoomMessageEventContent::new())
        .await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    let redacted_event_id = item.as_event().unwrap().event_id().unwrap();

    let edit = assign!(RoomMessageEventContent::text_plain(" * test"), {
        relates_to: Some(message::Relation::Replacement(Replacement::new(
            redacted_event_id.to_owned(),
            MessageType::text_plain("test").into(),
        ))),
    });
    timeline.handle_live_message_event(&ALICE, edit).await;

    assert_eq!(timeline.inner.items().await.len(), 2);

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());
}

#[async_test]
async fn test_live_sanitized() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_message_event(
            &ALICE,
            RoomMessageEventContent::text_html(
                "**original** message",
                "<strong>original</strong> message",
            ),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(message) = first_event.content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "**original** message");
    assert_eq!(text.formatted.as_ref().unwrap().body, "<strong>original</strong> message");

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    let first_event_id = first_event.event_id().unwrap();

    let new_plain_content = "!!edited!! **better** message";
    let new_html_content = "<edited/> <strong>better</strong> message";
    let edit = assign!(
        RoomMessageEventContent::text_html(
            format!("* {}", new_plain_content),
            format!("* {}", new_html_content)
        ),
        {
            relates_to: Some(message::Relation::Replacement(Replacement::new(
                first_event_id.to_owned(),
                MessageType::text_html(new_plain_content, new_html_content).into(),
            ))),
        }
    );
    timeline.handle_live_message_event(&ALICE, edit).await;

    let item = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let first_event = item.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(message) = first_event.content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, new_plain_content);
    assert_eq!(text.formatted.as_ref().unwrap().body, " <strong>better</strong> message");
}

#[async_test]
async fn test_aggregated_sanitized() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let original_event_id = EventId::new(server_name!("dummy.server"));
    let ev = sync_timeline_event!({
        "content": {
            "formatted_body": "<strong>original</strong> message",
            "format": "org.matrix.custom.html",
            "body": "**original** message",
            "msgtype": "m.text"
        },
        "event_id": &original_event_id,
        "origin_server_ts": timeline.event_builder.next_server_ts(),
        "sender": *ALICE,
        "type": "m.room.message",
        "unsigned": {
            "m.relations": {
                "m.replace": {
                    "content": {
                        "formatted_body": "* <edited/> <strong>better</strong> message",
                        "format": "org.matrix.custom.html",
                        "body": "* !!edited!! **better** message",
                        "m.new_content": {
                            "formatted_body": "<edited/> <strong>better</strong> message",
                            "format": "org.matrix.custom.html",
                            "body": "!!edited!! **better** message",
                            "msgtype": "m.text"
                        },
                        "m.relates_to": {
                            "event_id": original_event_id,
                            "rel_type": "m.replace"
                        },
                        "msgtype": "m.text"
                    },
                    "event_id": EventId::new(server_name!("dummy.server")),
                    "origin_server_ts": timeline.event_builder.next_server_ts(),
                    "sender": *ALICE,
                    "type": "m.room.message",
                }
            }
        }
    });
    timeline.handle_live_event(ev).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(message) = first_event.content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "!!edited!! **better** message");
    assert_eq!(text.formatted.as_ref().unwrap().body, " <strong>better</strong> message");

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());
}
