use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use im::vector;
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use matrix_sdk_test::async_test;
use ruma::{
    assign,
    events::{
        reaction::ReactionEventContent,
        relation::{Annotation, Replacement},
        room::{
            member::{MembershipState, RedactedRoomMemberEventContent, RoomMemberEventContent},
            message::{
                self, MessageType, RedactedRoomMessageEventContent, RoomMessageEventContent,
            },
            name::RoomNameEventContent,
            topic::RedactedRoomTopicEventContent,
        },
        FullStateEventContent,
    },
};
use serde_json::{json, Value as JsonValue};

use super::{TestTimeline, ALICE, BOB};
use crate::room::timeline::{
    event_item::AnyOtherFullStateEventContent, MembershipChange, TimelineItem, TimelineItemContent,
    VirtualTimelineItem,
};

fn sync_timeline_event(event: JsonValue) -> SyncTimelineEvent {
    let event = serde_json::from_value(event).unwrap();
    SyncTimelineEvent { event, encryption_info: None, push_actions: Vec::default() }
}

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

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    assert_matches!(&*item, TimelineItem::Virtual(VirtualTimelineItem::DayDivider(_)));
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    assert_eq!(item.as_event().unwrap().sender(), *ALICE);
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    assert_eq!(item.as_event().unwrap().sender(), *BOB);
}

#[async_test]
async fn reaction_redaction() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("hi!")).await;
    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event.reactions().len(), 0);

    let msg_event_id = event.event_id();

    let rel = Annotation::new(msg_event_id.to_owned(), "+1".to_owned());
    timeline.handle_live_message_event(&BOB, ReactionEventContent::new(rel)).await;
    let item =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 1, value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event.reactions().len(), 1);

    // TODO: After adding raw timeline items, check for one here

    let reaction_event_id = event.event_id();

    timeline.handle_live_redaction(&BOB, reaction_event_id).await;
    let item =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 1, value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event.reactions().len(), 0);
}

#[async_test]
async fn edit_redacted() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    timeline
        .handle_live_redacted_message_event(*ALICE, RedactedRoomMessageEventContent::new())
        .await;
    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);

    let redacted_event_id = item.as_event().unwrap().event_id().unwrap();

    let edit = assign!(RoomMessageEventContent::text_plain(" * test"), {
        relates_to: Some(message::Relation::Replacement(Replacement::new(
            redacted_event_id.to_owned(),
            MessageType::text_plain("test"),
        ))),
    });
    timeline.handle_live_message_event(&ALICE, edit).await;

    assert_eq!(timeline.inner.items().await.len(), 2);
}

#[async_test]
async fn sticker() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

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

    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::Sticker(_));
}

#[async_test]
async fn room_member() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

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

    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let membership = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::MembershipChange(ev) => ev);
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

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let membership = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::MembershipChange(ev) => ev);
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

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let profile = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::ProfileChange(ev) => ev);
    assert_matches!(profile.displayname_change(), Some(_));
    assert_matches!(profile.avatar_url_change(), None);

    timeline
        .handle_live_redacted_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            RedactedRoomMemberEventContent::new(MembershipState::Join),
        )
        .await;

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let membership = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::MembershipChange(ev) => ev);
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

    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let ev = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::OtherState(ev) => ev);
    let full_content =
        assert_matches!(ev.content(), AnyOtherFullStateEventContent::RoomName(c) => c);
    let (content, prev_content) = assert_matches!(full_content, FullStateEventContent::Original { content, prev_content } => (content, prev_content));
    assert_eq!(content.name.as_ref().unwrap(), "Alice's room");
    assert_matches!(prev_content, None);

    timeline.handle_live_redacted_state_event(&ALICE, RedactedRoomTopicEventContent::new()).await;

    let item = assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
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
    assert_matches!(*timeline_items[0], TimelineItem::Virtual(VirtualTimelineItem::DayDivider(_)));
    assert_matches!(*timeline_items[1], TimelineItem::Event(_));
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
