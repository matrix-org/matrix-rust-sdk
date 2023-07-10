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
use imbl::vector;
use matrix_sdk_test::async_test;
use ruma::{
    assign,
    events::{
        relation::{InReplyTo, Thread},
        room::{
            member::{MembershipState, RedactedRoomMemberEventContent, RoomMemberEventContent},
            message::{MessageType, Relation, RoomMessageEventContent},
            name::RoomNameEventContent,
            topic::RedactedRoomTopicEventContent,
        },
        FullStateEventContent,
    },
};
use serde_json::json;
use stream_assert::assert_next_matches;

use super::{sync_timeline_event, TestTimeline, ALICE, BOB};
use crate::timeline::{
    event_item::AnyOtherFullStateEventContent, MembershipChange, TimelineDetails,
    TimelineItemContent, TimelineItemKind, VirtualTimelineItem,
};

#[async_test]
async fn initial_events() {
    let mut timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .inner
        .add_initial_events(vector![
            sync_timeline_event(
                timeline.make_message_event(*ALICE, RoomMessageEventContent::text_plain("A")),
            ),
            sync_timeline_event(
                timeline.make_message_event(*BOB, RoomMessageEventContent::text_plain("B")),
            ),
        ])
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_matches!(&item.kind, TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_)));
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.as_event().unwrap().sender(), *ALICE);
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.as_event().unwrap().sender(), *BOB);
}

#[async_test]
async fn sticker() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    timeline
        .handle_live_custom_event(json!({
            "content": {
                "body": "Happy sticker",
                "info": {
                    "h": 398,
                    "mimetype": "image/jpeg",
                    "size": 31037,
                    "w": 394
                },
                "url": "mxc://server.name/JWEIFJgwEIhweiWJE",
            },
            "event_id": "$143273582443PhrSn",
            "origin_server_ts": 143273582,
            "sender": "@alice:server.name",
            "type": "m.sticker",
        }))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_matches!(item.content(), TimelineItemContent::Sticker(_));
}

#[async_test]
async fn room_member() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let mut first_room_member_content = RoomMemberEventContent::new(MembershipState::Invite);
    first_room_member_content.displayname = Some("Alice".to_owned());
    timeline
        .handle_live_state_event_with_state_key(
            &BOB,
            ALICE.to_owned(),
            first_room_member_content.clone(),
            None,
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let membership =
        assert_matches!(item.content(), TimelineItemContent::MembershipChange(ev) => ev);
    assert_matches!(membership.content(), FullStateEventContent::Original { .. });
    assert_matches!(membership.change(), Some(MembershipChange::Invited));

    let mut second_room_member_content = RoomMemberEventContent::new(MembershipState::Join);
    second_room_member_content.displayname = Some("Alice".to_owned());
    timeline
        .handle_live_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            second_room_member_content.clone(),
            Some(first_room_member_content),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let membership =
        assert_matches!(item.content(), TimelineItemContent::MembershipChange(ev) => ev);
    assert_matches!(membership.content(), FullStateEventContent::Original { .. });
    assert_matches!(membership.change(), Some(MembershipChange::InvitationAccepted));

    let mut third_room_member_content = RoomMemberEventContent::new(MembershipState::Join);
    third_room_member_content.displayname = Some("Alice In Wonderland".to_owned());
    timeline
        .handle_live_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            third_room_member_content,
            Some(second_room_member_content),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let profile = assert_matches!(item.content(), TimelineItemContent::ProfileChange(ev) => ev);
    assert_matches!(profile.displayname_change(), Some(_));
    assert_matches!(profile.avatar_url_change(), None);

    timeline
        .handle_live_redacted_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            RedactedRoomMemberEventContent::new(MembershipState::Join),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let membership =
        assert_matches!(item.content(), TimelineItemContent::MembershipChange(ev) => ev);
    assert_matches!(membership.content(), FullStateEventContent::Redacted(_));
    assert_matches!(membership.change(), None);
}

#[async_test]
async fn other_state() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_state_event(
            &ALICE,
            RoomNameEventContent::new(Some("Alice's room".to_owned())),
            None,
        )
        .await;

    let _day_divider = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let ev = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::OtherState(ev) => ev);
    let full_content =
        assert_matches!(ev.content(), AnyOtherFullStateEventContent::RoomName(c) => c);
    let (content, prev_content) = assert_matches!(full_content, FullStateEventContent::Original { content, prev_content } => (content, prev_content));
    assert_eq!(content.name.as_ref().unwrap(), "Alice's room");
    assert_matches!(prev_content, None);

    timeline.handle_live_redacted_state_event(&ALICE, RedactedRoomTopicEventContent::new()).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let ev = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::OtherState(ev) => ev);
    let full_content =
        assert_matches!(ev.content(), AnyOtherFullStateEventContent::RoomTopic(c) => c);
    assert_matches!(full_content, FullStateEventContent::Redacted(_));
}

#[async_test]
async fn dedup_pagination() {
    let timeline = TestTimeline::new();

    let event = timeline.make_message_event(*ALICE, RoomMessageEventContent::text_plain("o/"));
    timeline.handle_live_custom_event(event.clone()).await;
    timeline.handle_back_paginated_custom_event(event).await;

    let timeline_items = timeline.inner.items().await;
    assert_eq!(timeline_items.len(), 2);
    assert_matches!(
        timeline_items[0].kind,
        TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_))
    );
    assert_matches!(timeline_items[1].kind, TimelineItemKind::Event(_));
}

#[async_test]
async fn dedup_initial() {
    let mut timeline = TestTimeline::new();

    let event_a = sync_timeline_event(
        timeline.make_message_event(*ALICE, RoomMessageEventContent::text_plain("A")),
    );
    let event_b = sync_timeline_event(
        timeline.make_message_event(*BOB, RoomMessageEventContent::text_plain("B")),
    );

    timeline.inner.add_initial_events(vector![event_a.clone(), event_b, event_a]).await;

    let timeline_items = timeline.inner.items().await;
    assert_eq!(timeline_items.len(), 3);
    assert_eq!(timeline_items[1].as_event().unwrap().sender(), *BOB);
    assert_eq!(timeline_items[2].as_event().unwrap().sender(), *ALICE);
}

#[async_test]
async fn sanitized() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_message_event(
            &ALICE,
            RoomMessageEventContent::text_html(
                "\
                    @@Unknown text@@
                    Some text\n\n\
                    !!code```
                        Some code
                    ```
                ",
                "\
                    <unknown>Unknown text</unknown>\
                    <p>Some text</p>\
                    <code unknown=\"code\">Some code</code>\
                ",
            ),
        )
        .await;

    let _day_divider = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event = item.as_event().unwrap();
    let message = assert_matches!(event.content(), TimelineItemContent::Message(msg) => msg);
    let text = assert_matches!(message.msgtype(), MessageType::Text(text) => text);
    assert_eq!(
        text.formatted.as_ref().unwrap().body,
        "\
            Unknown text\
            <p>Some text</p>\
            <code>Some code</code>\
        "
    );
}

#[async_test]
async fn reply() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_message_event(
            &ALICE,
            RoomMessageEventContent::text_plain("I want you to reply"),
        )
        .await;

    let _day_divider = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    let first_event_id = first_event.event_id().unwrap();
    let first_event_sender = *ALICE;

    let reply_formatted_body = format!("\
        <mx-reply>\
            <blockquote>\
                <a href=\"https://matrix.to/#/!my_room:server.name/{first_event_id}\">In reply to</a> \
                <a href=\"https://matrix.to/#/{first_event_sender}\">{first_event_sender}</a>\
                <br>\
                I want you to reply\
            </blockquote>\
        </mx-reply>\
        <p>I'm replying!</p>\
    ");
    let reply_plain = format!(
        "> <{first_event_sender}> I want you to reply\n\
         I'm replying!"
    );
    let reply = assign!(RoomMessageEventContent::text_html(reply_plain, reply_formatted_body), {
        relates_to: Some(Relation::Reply {
            in_reply_to: InReplyTo::new(first_event_id.to_owned()),
        }),
    });

    timeline.handle_live_message_event(&BOB, reply).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let message = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::Message(msg) => msg);

    let text = assert_matches!(message.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "I'm replying!");
    assert_eq!(text.formatted.as_ref().unwrap().body, "<p>I'm replying!</p>");

    let in_reply_to = message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, first_event_id);
    let replied_to_event = assert_matches!(&in_reply_to.event, TimelineDetails::Ready(msg) => msg);
    assert_eq!(replied_to_event.sender(), *ALICE);
}

#[async_test]
async fn thread() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_message_event(
            &ALICE,
            RoomMessageEventContent::text_plain("I want you to reply"),
        )
        .await;

    let _day_divider = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    let first_event_id = first_event.event_id().unwrap();

    let reply = assign!(RoomMessageEventContent::text_plain("I'm replying in a thread"), {
        relates_to: Some(Relation::Thread(
            Thread::plain(first_event_id.to_owned(), first_event_id.to_owned()),
        ))
    });

    timeline.handle_live_message_event(&BOB, reply).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let message = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::Message(msg) => msg);

    let text = assert_matches!(message.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "I'm replying in a thread");
    assert_matches!(text.formatted, None);

    let in_reply_to = message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, first_event_id);
    let replied_to_event = assert_matches!(&in_reply_to.event, TimelineDetails::Ready(msg) => msg);
    assert_eq!(replied_to_event.sender(), *ALICE);
}
