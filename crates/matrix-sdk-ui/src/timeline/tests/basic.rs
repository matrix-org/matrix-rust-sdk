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
use futures_util::StreamExt;
use imbl::vector;
use matrix_sdk::{assert_next_with_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_base::ThreadingSupport;
use matrix_sdk_test::{
    ALICE, BOB, CAROL, JoinedRoomBuilder, async_test,
    event_factory::{EventFactory, PreviousMembership},
};
use ruma::{
    MilliSecondsSinceUnixEpoch, event_id,
    events::{
        FullStateEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::{
            ImageInfo,
            member::{MembershipState, RedactedRoomMemberEventContent},
            message::MessageType,
            topic::RedactedRoomTopicEventContent,
        },
    },
    mxc_uri, owned_event_id, owned_mxc_uri, room_id, user_id,
};
use stream_assert::{assert_next_matches, assert_pending};

use super::TestTimeline;
use crate::timeline::{
    MembershipChange, MsgLikeContent, MsgLikeKind, RoomExt, TimelineDetails, TimelineFocus,
    TimelineItemContent, TimelineItemKind, TimelineReadReceiptTracking, VirtualTimelineItem,
    controller::TimelineSettings,
    event_item::{AnyOtherFullStateEventContent, RemoteEventOrigin},
    tests::{ReadReceiptMap, TestRoomDataProvider, TestTimelineBuilder},
};

#[async_test]
async fn test_initial_events() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline
        .controller
        .handle_remote_events_with_diffs(
            vec![VectorDiff::Append {
                values: vector![
                    f.text_msg("A").sender(*ALICE).into_event(),
                    f.text_msg("B").sender(*BOB).into_event()
                ],
            }],
            RemoteEventOrigin::Sync,
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.as_event().unwrap().sender(), *ALICE);

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_eq!(item.as_event().unwrap().sender(), *BOB);

    let item = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert_matches!(&item.kind, TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(_)));
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

    let timeline = TestTimelineBuilder::new()
        .provider(
            TestRoomDataProvider::default()
                // Also add a fully read marker.
                .with_fully_read_marker(event_id)
                .with_initial_user_receipts(receipts),
        )
        .settings(TimelineSettings {
            track_read_receipts: TimelineReadReceiptTracking::AllEvents,
            ..Default::default()
        })
        .build();

    let f = &timeline.factory;
    let ev = f.text_msg("hey").sender(*ALICE).into_event();

    timeline
        .controller
        .handle_remote_events_with_diffs(
            vec![VectorDiff::Append { values: vector![ev] }],
            RemoteEventOrigin::Sync,
        )
        .await;

    let items = timeline.controller.items().await;
    assert_eq!(items.len(), 2);
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "hey");

    let ev = f.text_msg("yo").sender(*BOB).into_event();
    timeline.controller.replace_with_initial_remote_events([ev], RemoteEventOrigin::Sync).await;

    let items = timeline.controller.items().await;
    assert_eq!(items.len(), 2);
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "yo");
}

#[async_test]
async fn test_sticker() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    timeline
        .handle_live_event(
            EventFactory::new()
                .sticker(
                    "Happy sticker",
                    ImageInfo::new(),
                    owned_mxc_uri!("mxc://server.name/JWEIFJgwEIhweiWJE"),
                )
                .reply_thread(event_id!("$thread_root"), event_id!("$in_reply_to"))
                .event_id(event_id!("$143273582443PhrSn"))
                .sender(user_id!("@alice:server.name"))
                .into_event(),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert!(item.content().is_sticker());

    assert_eq!(item.content().thread_root(), Some(event_id!("$thread_root").to_owned()));

    assert_let!(Some(details) = item.content().in_reply_to());
    assert_eq!(details.event_id, event_id!("$in_reply_to").to_owned())
}

#[async_test]
async fn test_room_member() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    // Bob invites Alice.
    let f = &timeline.factory;
    timeline.handle_live_event(f.member(&BOB).invited(&ALICE).display_name("Alice")).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert!(item.can_be_replied_to());
    assert_let!(TimelineItemContent::MembershipChange(membership) = item.content());
    assert_matches!(membership.content(), FullStateEventContent::Original { .. });
    assert_matches!(membership.change(), Some(MembershipChange::Invited));

    timeline
        .handle_live_event(
            f.member(&ALICE)
                .membership(MembershipState::Join)
                .display_name("Alice")
                .previous(PreviousMembership::new(MembershipState::Invite).display_name("Alice")),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::MembershipChange(membership) = item.content());
    assert_matches!(membership.content(), FullStateEventContent::Original { .. });
    assert_matches!(membership.change(), Some(MembershipChange::InvitationAccepted));

    timeline
        .handle_live_event(
            f.member(&ALICE)
                .membership(MembershipState::Join)
                .display_name("Alice In Wonderland")
                .previous(PreviousMembership::new(MembershipState::Join).display_name("Alice")),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::ProfileChange(profile) = item.content());
    assert_matches!(profile.displayname_change(), Some(_));
    assert_matches!(profile.avatar_url_change(), None);

    timeline
        .handle_live_event(
            f.member(&ALICE)
                .membership(MembershipState::Join)
                .display_name("Alice In Wonderland")
                .avatar_url(mxc_uri!("mxc://lolcathost.io/abc"))
                .previous(
                    PreviousMembership::new(MembershipState::Join)
                        .display_name("Alice In Wonderland"),
                ),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::ProfileChange(profile) = item.content());
    assert_matches!(profile.displayname_change(), None);
    assert_matches!(profile.avatar_url_change(), Some(_));

    {
        // No avatar or display name in the new room member event content, but it's
        // possible to get the previous one using the getters.
        timeline
            .handle_live_event(
                f.member(&ALICE).membership(MembershipState::Leave).previous(
                    PreviousMembership::new(MembershipState::Join)
                        .display_name("Alice In Wonderland")
                        .avatar_url(mxc_uri!("mxc://lolcathost.io/abc")),
                ),
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
        .handle_live_event(f.redacted_state(
            &ALICE,
            ALICE.as_str(),
            RedactedRoomMemberEventContent::new(MembershipState::Join),
        ))
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

    let f = &timeline.factory;
    timeline.handle_live_event(f.room_name("Alice's room").sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::OtherState(ev) = item.as_event().unwrap().content());
    assert_let!(AnyOtherFullStateEventContent::RoomName(full_content) = ev.content());
    assert_let!(FullStateEventContent::Original { content, prev_content } = full_content);
    assert_eq!(content.name, "Alice's room");
    assert_matches!(prev_content, None);

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    timeline
        .handle_live_event(f.redacted_state(&ALICE, "", RedactedRoomTopicEventContent::new()))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(TimelineItemContent::OtherState(ev) = item.as_event().unwrap().content());
    assert_let!(AnyOtherFullStateEventContent::RoomTopic(full_content) = ev.content());
    assert_matches!(full_content, FullStateEventContent::Redacted(_));
}

#[async_test]
async fn test_internal_id_prefix() {
    let timeline = TestTimelineBuilder::new().internal_id_prefix("le_prefix_".to_owned()).build();

    let f = &timeline.factory;
    let ev_a = f.text_msg("A").sender(*ALICE).into_event();
    let ev_b = f.text_msg("B").sender(*BOB).into_event();
    let ev_c = f.text_msg("C").sender(*CAROL).into_event();

    timeline
        .controller
        .handle_remote_events_with_diffs(
            vec![VectorDiff::Append { values: vector![ev_a, ev_b, ev_c] }],
            RemoteEventOrigin::Sync,
        )
        .await;

    let timeline_items = timeline.controller.items().await;
    assert_eq!(timeline_items.len(), 4);

    assert!(timeline_items[0].is_date_divider());
    assert_eq!(timeline_items[0].unique_id().0, "le_prefix_3");

    let event1 = &timeline_items[1];
    assert_eq!(event1.as_event().unwrap().sender(), *ALICE);
    assert_eq!(event1.unique_id().0, "le_prefix_0");

    let event2 = &timeline_items[2];
    assert_eq!(event2.as_event().unwrap().sender(), *BOB);
    assert_eq!(event2.unique_id().0, "le_prefix_1");

    let event3 = &timeline_items[3];
    assert_eq!(event3.as_event().unwrap().sender(), *CAROL);
    assert_eq!(event3.unique_id().0, "le_prefix_2");
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
    assert_let!(Some(message) = event.content().as_message());
    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(
        text.formatted.as_ref().unwrap().body,
        "\
            Unknown text\
            <p>Some text</p>\
            <code>Some code</code>\
        "
    );

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());
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

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    let reply_formatted_body = format!(
        "\
            <mx-reply>\
                <blockquote>\
                    <a href=\"https://matrix.to/#/!my_room:server.name/{first_event_id}\">In reply to</a> \
                    <a href=\"https://matrix.to/#/{first_event_sender}\">{first_event_sender}</a>\
                    <br>\
                    I want you to reply\
                </blockquote>\
            </mx-reply>\
            <p>I'm replying!</p>\
        "
    );
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
    assert_let!(
        TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::Message(message),
            in_reply_to,
            ..
        }) = item.as_event().unwrap().content()
    );

    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "I'm replying!");
    assert_eq!(text.formatted.as_ref().unwrap().body, "<p>I'm replying!</p>");

    let in_reply_to = in_reply_to.clone().unwrap();
    assert_eq!(in_reply_to.event_id, first_event_id);
    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_eq!(replied_to_event.sender, *ALICE);
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

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    timeline
        .handle_live_event(
            f.text_msg("I'm replying in a thread")
                .sender(&BOB)
                .in_thread(first_event_id, first_event_id),
        )
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    assert_let!(
        TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::Message(message),
            in_reply_to,
            ..
        }) = item.as_event().unwrap().content()
    );

    assert_let!(MessageType::Text(text) = message.msgtype());
    assert_eq!(text.body, "I'm replying in a thread");
    assert_matches!(text.formatted, None);

    let in_reply_to = in_reply_to.clone().unwrap();
    assert_eq!(in_reply_to.event_id, first_event_id);
    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_eq!(replied_to_event.sender, *ALICE);
}

#[async_test]
async fn test_replace_with_initial_events_when_batched() {
    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default())
        .settings(TimelineSettings::default())
        .build();

    let f = &timeline.factory;
    let ev = f.text_msg("hey").sender(*ALICE).into_event();

    timeline
        .controller
        .handle_remote_events_with_diffs(
            vec![VectorDiff::Append { values: vector![ev] }],
            RemoteEventOrigin::Sync,
        )
        .await;

    let (items, mut stream) = timeline.controller.subscribe().await;
    assert_eq!(items.len(), 2);
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "hey");

    let ev = f.text_msg("yo").sender(*BOB).into_event();
    timeline.controller.replace_with_initial_remote_events([ev], RemoteEventOrigin::Sync).await;

    // Assert there are more than a single Clear diff in the next batch:
    // Clear + PushBack (event) + PushFront (date divider)
    let batched_diffs = stream.next().await.unwrap();
    assert_eq!(batched_diffs.len(), 3);
    assert_matches!(batched_diffs[0], VectorDiff::Clear);
    assert_matches!(&batched_diffs[1], VectorDiff::PushBack { value } => {
        assert!(value.as_event().is_some());
    });
    assert_matches!(&batched_diffs[2], VectorDiff::PushFront { value } => {
        assert_matches!(value.as_virtual(), Some(VirtualTimelineItem::DateDivider(_)));
    });
}

#[async_test]
async fn test_latest_event_id_in_main_timeline() {
    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|b| {
            b.with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
        })
        .build()
        .await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let event_id = event_id!("$message");
    let reaction_event_id = event_id!("$reaction");
    let thread_event_id = event_id!("$thread");

    let room = server.sync_joined_room(&client, room_id).await;
    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: true })
        .track_read_marker_and_receipts(TimelineReadReceiptTracking::AllEvents)
        .build()
        .await
        .expect("Could not build live timeline");

    let (items, mut stream) = timeline.subscribe().await;
    assert!(items.is_empty());
    assert_pending!(stream);

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    // If we receive a message in the live timeline
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("A message").event_id(event_id).into_raw_sync()),
        )
        .await;

    let items = assert_next_with_timeout!(stream);
    // We receive the day divider and the items
    assert_eq!(items.len(), 2);
    assert_let!(Some(latest_event_id) = timeline.controller.latest_event_id().await);
    // The latest event id is from the event in the live timeline
    assert_eq!(event_id, latest_event_id);

    // If we then receive a reaction
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.reaction(event_id, ":D").event_id(reaction_event_id).into_raw_sync(),
            ),
        )
        .await;

    let items = assert_next_with_timeout!(stream);
    // The reaction is added
    assert_eq!(items.len(), 1);
    assert_let!(Some(latest_event_id) = timeline.controller.latest_event_id().await);
    // And it's now the latest event id
    assert_eq!(reaction_event_id, latest_event_id);

    // If we now get a message in a thread inside that room
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("In thread")
                    .in_thread(event_id, thread_event_id)
                    .event_id(thread_event_id)
                    .into_raw_sync(),
            ),
        )
        .await;
    // The thread root event is updated
    assert_eq!(items.len(), 1);
    assert_let!(Some(latest_event_id) = timeline.controller.latest_event_id().await);

    // But the latest event in the live timeline is still the reaction, since the
    // threaded event is not part of the live timeline
    assert_eq!(reaction_event_id, latest_event_id);
}
