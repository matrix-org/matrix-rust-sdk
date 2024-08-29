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
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk_test::{async_test, sync_timeline_event, ALICE, BOB, CAROL};
use ruma::{
    events::{
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::{
            member::{MembershipState, RedactedRoomMemberEventContent, RoomMemberEventContent},
            message::{MessageType, RoomMessageEventContent},
            name::RoomNameEventContent,
            topic::RedactedRoomTopicEventContent,
        },
        FullStateEventContent,
    },
    owned_event_id, owned_mxc_uri, MilliSecondsSinceUnixEpoch,
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::{
    controller::{TimelineEnd, TimelineSettings},
    event_item::{AnyOtherFullStateEventContent, RemoteEventOrigin},
    tests::{ReadReceiptMap, TestRoomDataProvider},
    MembershipChange, TimelineDetails, TimelineItemContent, TimelineItemKind, VirtualTimelineItem,
};

#[async_test]
async fn test_initial_events() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline
        .controller
        .add_events_at(
            vec![f.text_msg("A").sender(*ALICE), f.text_msg("B").sender(*BOB)],
            TimelineEnd::Back,
            RemoteEventOrigin::Sync,
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.as_event().unwrap().sender(), *ALICE);

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.as_event().unwrap().sender(), *BOB);

    let item = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert_matches!(&item.kind, TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_)));
}

#[async_test]
async fn test_replace_with_initial_events_and_read_marker() {
    let event_id = owned_event_id!("$1");

    // Add a read receipt for the logged in user (Alice).
    let mut receipts = ReadReceiptMap::default();
    receipts
        .entry(ReceiptType::Read)
        .or_default()
        .entry(ReceiptThread::Unthreaded)
        .or_default()
        .entry(ALICE.to_owned())
        .or_insert_with(|| (event_id.to_owned(), Receipt::new(MilliSecondsSinceUnixEpoch::now())));

    let timeline = TestTimeline::with_room_data_provider(
        TestRoomDataProvider::default()
            // Also add a fully read marker.
            .with_fully_read_marker(event_id)
            .with_initial_user_receipts(receipts),
    )
    .with_settings(TimelineSettings { track_read_receipts: true, ..Default::default() });

    let f = &timeline.factory;
    let ev = f.text_msg("hey").sender(*ALICE).into_sync();

    timeline.controller.add_events_at(vec![ev], TimelineEnd::Back, RemoteEventOrigin::Sync).await;

    let items = timeline.controller.items().await;
    assert_eq!(items.len(), 2);
    assert!(items[0].is_day_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "hey");

    let ev = f.text_msg("yo").sender(*BOB).into_sync();
    timeline.controller.replace_with_initial_remote_events(vec![ev], RemoteEventOrigin::Sync).await;

    let items = timeline.controller.items().await;
    assert_eq!(items.len(), 2);
    assert!(items[0].is_day_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "yo");
}

#[async_test]
async fn test_sticker() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    timeline
        .handle_live_event(sync_timeline_event!({
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
async fn test_room_member() {
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
    assert!(item.can_be_replied_to());
    assert_let!(TimelineItemContent::MembershipChange(membership) = item.content());
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
    assert_let!(TimelineItemContent::MembershipChange(membership) = item.content());
    assert_matches!(membership.content(), FullStateEventContent::Original { .. });
    assert_matches!(membership.change(), Some(MembershipChange::InvitationAccepted));

    let mut third_room_member_content = RoomMemberEventContent::new(MembershipState::Join);
    third_room_member_content.displayname = Some("Alice In Wonderland".to_owned());
    timeline
        .handle_live_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            third_room_member_content.clone(),
            Some(second_room_member_content),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::ProfileChange(profile) = item.content());
    assert_matches!(profile.displayname_change(), Some(_));
    assert_matches!(profile.avatar_url_change(), None);

    let mut fourth_room_member_content = RoomMemberEventContent::new(MembershipState::Join);
    fourth_room_member_content.displayname = Some("Alice In Wonderland".to_owned());
    fourth_room_member_content.avatar_url = Some(owned_mxc_uri!("mxc://lolcathost.io/abc"));
    timeline
        .handle_live_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            fourth_room_member_content.clone(),
            Some(third_room_member_content),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::ProfileChange(profile) = item.content());
    assert_matches!(profile.displayname_change(), None);
    assert_matches!(profile.avatar_url_change(), Some(_));

    {
        // No avatar or display name in the new room member event content, but it's
        // possible to get the previous one using the getters.
        let room_member_content = RoomMemberEventContent::new(MembershipState::Leave);

        timeline
            .handle_live_state_event_with_state_key(
                &ALICE,
                ALICE.to_owned(),
                room_member_content,
                Some(fourth_room_member_content),
            )
            .await;

        let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
        assert_let!(TimelineItemContent::MembershipChange(membership) = item.content());
        assert_matches!(membership.display_name().as_deref(), Some("Alice In Wonderland"));
        assert_matches!(
            membership.avatar_url().map(|url| url.to_string()).as_deref(),
            Some("mxc://lolcathost.io/abc")
        );
        assert_matches!(membership.content(), FullStateEventContent::Original { .. });
        assert_matches!(membership.change(), Some(MembershipChange::Left));
    }

    timeline
        .handle_live_redacted_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            RedactedRoomMemberEventContent::new(MembershipState::Join),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::MembershipChange(membership) = item.content());
    assert_matches!(membership.content(), FullStateEventContent::Redacted(_));
    assert_matches!(membership.change(), None);
}

#[async_test]
async fn test_other_state() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_state_event(&ALICE, RoomNameEventContent::new("Alice's room".to_owned()), None)
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::OtherState(ev) = item.as_event().unwrap().content());
    assert_let!(AnyOtherFullStateEventContent::RoomName(full_content) = ev.content());
    assert_let!(FullStateEventContent::Original { content, prev_content } = full_content);
    assert_eq!(content.name, "Alice's room");
    assert_matches!(prev_content, None);

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    timeline.handle_live_redacted_state_event(&ALICE, RedactedRoomTopicEventContent::new()).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::OtherState(ev) = item.as_event().unwrap().content());
    assert_let!(AnyOtherFullStateEventContent::RoomTopic(full_content) = ev.content());
    assert_matches!(full_content, FullStateEventContent::Redacted(_));
}

#[async_test]
async fn test_dedup_pagination() {
    let timeline = TestTimeline::new();

    let event = timeline
        .event_builder
        .make_sync_message_event(*ALICE, RoomMessageEventContent::text_plain("o/"));
    timeline.handle_live_event(event.clone()).await;
    // This cast is not actually correct, sync events aren't valid
    // back-paginated events, as they are missing `room_id`. However, the
    // timeline doesn't care about that `room_id` and casts back to
    // `Raw<AnySyncTimelineEvent>` before attempting to deserialize.
    timeline.handle_back_paginated_event(event.cast()).await;

    let timeline_items = timeline.controller.items().await;
    assert_eq!(timeline_items.len(), 2);
    assert_matches!(
        timeline_items[0].kind,
        TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_))
    );
    assert_matches!(timeline_items[1].kind, TimelineItemKind::Event(_));
}

#[async_test]
async fn test_dedup_initial() {
    let timeline = TestTimeline::new();

    let f = &timeline.factory;
    let event_a = f.text_msg("A").sender(*ALICE).into_sync();
    let event_b = f.text_msg("B").sender(*BOB).into_sync();
    let event_c = f.text_msg("C").sender(*CAROL).into_sync();

    timeline
        .controller
        .add_events_at(
            vec![
                // two events
                event_a.clone(),
                event_b.clone(),
                // same events got duplicated in next sync response
                event_a,
                event_b,
                // … and a new event also came in
                event_c,
            ],
            TimelineEnd::Back,
            RemoteEventOrigin::Sync,
        )
        .await;

    let timeline_items = timeline.controller.items().await;
    assert_eq!(timeline_items.len(), 4);

    assert!(timeline_items[0].is_day_divider());

    let event1 = &timeline_items[1];
    let event2 = &timeline_items[2];
    let event3 = &timeline_items[3];

    // Make sure the order is right
    assert_eq!(event1.as_event().unwrap().sender(), *ALICE);
    assert_eq!(event2.as_event().unwrap().sender(), *BOB);
    assert_eq!(event3.as_event().unwrap().sender(), *CAROL);

    // Make sure we reused IDs when deduplicating events
    assert_eq!(event1.unique_id(), "0");
    assert_eq!(event2.unique_id(), "1");
    assert_eq!(event3.unique_id(), "2");
    assert_eq!(timeline_items[0].unique_id(), "3");
}

#[async_test]
async fn test_internal_id_prefix() {
    let timeline = TestTimeline::with_internal_id_prefix("le_prefix_".to_owned());

    let f = &timeline.factory;
    let ev_a = f.text_msg("A").sender(*ALICE).into_sync();
    let ev_b = f.text_msg("B").sender(*BOB).into_sync();
    let ev_c = f.text_msg("C").sender(*CAROL).into_sync();

    timeline
        .controller
        .add_events_at(vec![ev_a, ev_b, ev_c], TimelineEnd::Back, RemoteEventOrigin::Sync)
        .await;

    let timeline_items = timeline.controller.items().await;
    assert_eq!(timeline_items.len(), 4);

    assert!(timeline_items[0].is_day_divider());
    assert_eq!(timeline_items[0].unique_id(), "le_prefix_3");

    let event1 = &timeline_items[1];
    assert_eq!(event1.as_event().unwrap().sender(), *ALICE);
    assert_eq!(event1.unique_id(), "le_prefix_0");

    let event2 = &timeline_items[2];
    assert_eq!(event2.as_event().unwrap().sender(), *BOB);
    assert_eq!(event2.unique_id(), "le_prefix_1");

    let event3 = &timeline_items[3];
    assert_eq!(event3.as_event().unwrap().sender(), *CAROL);
    assert_eq!(event3.unique_id(), "le_prefix_2");
}

#[async_test]
async fn test_sanitized() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;

    timeline
        .handle_live_event(
            f.text_html(
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
            )
            .sender(&ALICE),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event = item.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(message) = event.content());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(
        text.formatted.as_ref().unwrap().body,
        "\
            Unknown text\
            <p>Some text</p>\
            <code>Some code</code>\
        "
    );

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());
}

#[async_test]
async fn test_reply() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("I want you to reply").sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    assert!(first_event.can_be_replied_to());
    let first_event_id = first_event.event_id().unwrap();
    let first_event_sender = *ALICE;

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

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

    timeline
        .handle_live_event(
            f.text_html(reply_plain, reply_formatted_body).reply_to(first_event_id).sender(&BOB),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::Message(message) = item.as_event().unwrap().content());

    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "I'm replying!");
    assert_eq!(text.formatted.as_ref().unwrap().body, "<p>I'm replying!</p>");

    let in_reply_to = message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, first_event_id);
    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_eq!(replied_to_event.sender(), *ALICE);
}

#[async_test]
async fn test_thread() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("I want you to reply").sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event = item.as_event().unwrap();
    let first_event_id = first_event.event_id().unwrap();

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    timeline
        .handle_live_event(
            f.text_msg("I'm replying in a thread")
                .sender(&BOB)
                .in_thread(first_event_id, first_event_id),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::Message(message) = item.as_event().unwrap().content());

    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "I'm replying in a thread");
    assert_matches!(text.formatted, None);

    let in_reply_to = message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, first_event_id);
    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_eq!(replied_to_event.sender(), *ALICE);
}
