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
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    config::SyncSettings,
    test_utils::{events::EventFactory, logged_in_client_with_server},
};
use matrix_sdk_test::{
    async_test, mocks::mock_encryption_state, sync_timeline_event, EventBuilder,
    GlobalAccountDataTestEvent, JoinedRoomBuilder, SyncResponseBuilder, ALICE, BOB,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineDetails, TimelineItemContent};
use ruma::{
    event_id,
    events::room::{
        member::{MembershipState, RoomMemberEventContent},
        message::{MessageType, RoomMessageEventContent},
    },
    room_id, user_id,
};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};

use crate::mock_sync;

#[async_test]
async fn test_batched() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().event_filter(|_, _| true).build().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe_batched().await;

    let hdl = tokio::spawn(async move {
        let next_batch = timeline_stream.next().await.unwrap();
        // There can be more than three updates because we add things like
        // day dividers and implicit read receipts
        assert!(next_batch.len() >= 3);
    });

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(event_builder.make_sync_message_event(
                &ALICE,
                RoomMessageEventContent::text_plain("text message event"),
            ))
            .add_timeline_event(event_builder.make_sync_state_event(
                &BOB,
                BOB.as_str(),
                RoomMemberEventContent::new(MembershipState::Join),
                Some(RoomMemberEventContent::new(MembershipState::Invite)),
            ))
            .add_timeline_event(event_builder.make_sync_message_event(
                &BOB,
                RoomMessageEventContent::notice_plain("notice message event"),
            )),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    tokio::time::timeout(Duration::from_millis(500), hdl).await.unwrap().unwrap();
}

#[async_test]
async fn test_event_filter() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().event_filter(|_, _| true).build().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let first_event_id = event_id!("$YTQwYl2ply");
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
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

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    let first_event = first.as_event().unwrap();
    assert_eq!(first_event.event_id(), Some(first_event_id));
    assert_eq!(first_event.read_receipts().len(), 1, "implicit read receipt");
    assert_let!(TimelineItemContent::Message(msg) = first_event.content());
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert!(!msg.is_edited());

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    let second_event_id = event_id!("$Ga6Y2l0gKY");
    let edit_event_id = event_id!("$7i9In0gEmB");
    sync_builder.add_joined_room(
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

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: second }) = timeline_stream.next().await);
    let second_event = second.as_event().unwrap();
    assert_eq!(second_event.event_id(), Some(second_event_id));
    assert_eq!(second_event.read_receipts().len(), 1, "implicit read receipt");

    // The implicit read receipt of Alice is moving from Alice's message...
    assert_let!(Some(VectorDiff::Set { index: 1, value: first }) = timeline_stream.next().await);
    assert_eq!(first.as_event().unwrap().read_receipts().len(), 0, "no more implicit read receipt");
    // … to Alice's edit. But since this item isn't visible, it's lost in the weeds!

    // The edit is applied to the first event.
    assert_let!(Some(VectorDiff::Set { index: 1, value: first }) = timeline_stream.next().await);
    let first_event = first.as_event().unwrap();
    assert!(first_event.read_receipts().is_empty());
    assert_let!(TimelineItemContent::Message(msg) = first_event.content());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "hi");
    assert!(msg.is_edited());
}

#[async_test]
async fn test_timeline_is_reset_when_a_user_is_ignored_or_unignored() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().build().await.unwrap();
    let (_, timeline_stream) = timeline.subscribe().await;
    pin_mut!(timeline_stream);

    let alice = user_id!("@alice:example.org");
    let bob = user_id!("@bob:example.org");

    let first_event_id = event_id!("$YTQwYl2pl1");
    let second_event_id = event_id!("$YTQwYl2pl2");
    let third_event_id = event_id!("$YTQwYl2pl3");

    let mut ev_factory = EventFactory::new().room(room_id);

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(ev_factory.text_msg("hello").sender(alice).event_id(first_event_id))
            .add_timeline_event(ev_factory.text_msg("hello").sender(bob).event_id(second_event_id))
            .add_timeline_event(
                ev_factory.text_msg("hello").sender(alice).event_id(third_event_id),
            ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(first_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(second_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(first_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(third_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });
    assert_pending!(timeline_stream);

    sync_builder.add_global_account_data_event(GlobalAccountDataTestEvent::Custom(json!({
        "content": {
            "ignored_users": {
                bob: {}
            }
        },
        "type": "m.ignored_user_list",
    })));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // The timeline has been emptied.
    assert_next_matches!(timeline_stream, VectorDiff::Clear);
    assert_pending!(timeline_stream);

    let fourth_event_id = event_id!("$YTQwYl2pl4");
    let fifth_event_id = event_id!("$YTQwYl2pl5");

    // All the next events are sent by Alice now.
    ev_factory = ev_factory.sender(alice);

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(ev_factory.text_msg("hello").event_id(fourth_event_id))
            .add_timeline_event(ev_factory.text_msg("hello").event_id(fifth_event_id)),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Timeline receives events as before.
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(fourth_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 0, value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(fourth_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(fifth_event_id));
    });
    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_profile_updates() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().build().await.unwrap();
    let (_, timeline_stream) = timeline.subscribe().await;
    pin_mut!(timeline_stream);

    let alice = "@alice:example.org";
    let bob = "@bob:example.org";

    // Add users with unknown profile.
    let event_1_id = event_id!("$YTQwYl2pl1");
    let event_2_id = event_id!("$YTQwYl2pl2");

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": event_1_id,
                "origin_server_ts": 152037280,
                "sender": alice,
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": event_2_id,
                "origin_server_ts": 152037280,
                "sender": bob,
                "type": "m.room.message",
            })),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let item_1 = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let event_1_item = item_1.as_event().unwrap();
    assert_eq!(event_1_item.event_id(), Some(event_1_id));
    assert_matches!(event_1_item.sender_profile(), TimelineDetails::Unavailable);

    let item_2 = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let event_2_item = item_2.as_event().unwrap();
    assert_eq!(event_2_item.event_id(), Some(event_2_id));
    assert_matches!(event_2_item.sender_profile(), TimelineDetails::Unavailable);

    assert_next_matches!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });

    assert_pending!(timeline_stream);

    // Add profiles of users and other message.
    let event_3_id = event_id!("$YTQwYl2pl3");
    let event_4_id = event_id!("$YTQwYl2pl4");
    let event_5_id = event_id!("$YTQwYl2pl5");

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "displayname": "Member",
                    "membership": "join"
                },
                "event_id": event_3_id,
                "origin_server_ts": 152037280,
                "sender": bob,
                "state_key": bob,
                "type": "m.room.member",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "displayname": "Alice",
                    "membership": "join"
                },
                "event_id": event_4_id,
                "origin_server_ts": 152037280,
                "sender": alice,
                "state_key": alice,
                "type": "m.room.member",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": event_5_id,
                "origin_server_ts": 152037280,
                "sender": alice,
                "type": "m.room.message",
            })),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Read receipt change.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 2, value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(event_2_id));
    });

    // The events are added.
    let item_3 = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let event_3_item = item_3.as_event().unwrap();
    assert_eq!(event_3_item.event_id(), Some(event_3_id));
    let profile =
        assert_matches!(event_3_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(!profile.display_name_ambiguous);

    // Read receipt change.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(event_1_id));
    });

    let item_4 = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let event_4_item = item_4.as_event().unwrap();
    assert_eq!(event_4_item.event_id(), Some(event_4_id));
    let profile =
        assert_matches!(event_4_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Alice"));
    assert!(!profile.display_name_ambiguous);

    // Read receipt change.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 4, value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(event_4_id));
    });

    let item_5 = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let event_5_item = item_5.as_event().unwrap();
    assert_eq!(event_5_item.event_id(), Some(event_5_id));
    let profile =
        assert_matches!(event_5_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Alice"));
    assert!(!profile.display_name_ambiguous);

    // The profiles changed.
    let item_1 =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => value);
    let event_1_item = item_1.as_event().unwrap();
    assert_eq!(event_1_item.event_id(), Some(event_1_id));
    let profile =
        assert_matches!(event_1_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Alice"));
    assert!(!profile.display_name_ambiguous);

    let item_2 =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 2, value } => value);
    let event_2_item = item_2.as_event().unwrap();
    assert_eq!(event_2_item.event_id(), Some(event_2_id));
    let profile =
        assert_matches!(event_2_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(!profile.display_name_ambiguous);

    assert_pending!(timeline_stream);

    // Change name to be ambiguous.
    let event_6_id = event_id!("$YTQwYl2pl6");

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "displayname": "Member",
                "membership": "join"
            },
            "event_id": event_6_id,
            "origin_server_ts": 152037280,
            "sender": alice,
            "state_key": alice,
            "type": "m.room.member",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Read receipt change.
    assert_next_matches!(timeline_stream, VectorDiff::Set { index: 5, value } => {
        assert_eq!(value.as_event().unwrap().event_id(), Some(event_5_id));
    });

    // The event is added.
    let item_6 = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let event_6_item = item_6.as_event().unwrap();
    assert_eq!(event_6_item.event_id(), Some(event_6_id));
    let profile =
        assert_matches!(event_6_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    // The profiles changed.
    let item_1 =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 1, value } => value);
    let event_1_item = item_1.as_event().unwrap();
    assert_eq!(event_1_item.event_id(), Some(event_1_id));
    let profile =
        assert_matches!(event_1_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    let item_2 =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 2, value } => value);
    let event_2_item = item_2.as_event().unwrap();
    assert_eq!(event_2_item.event_id(), Some(event_2_id));
    let profile =
        assert_matches!(event_2_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    let item_3 =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 3, value } => value);
    let event_3_item = item_3.as_event().unwrap();
    assert_eq!(event_3_item.event_id(), Some(event_3_id));
    let profile =
        assert_matches!(event_3_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    let item_4 =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 4, value } => value);
    let event_4_item = item_4.as_event().unwrap();
    assert_eq!(event_4_item.event_id(), Some(event_4_id));
    let profile =
        assert_matches!(event_4_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    let item_5 =
        assert_next_matches!(timeline_stream, VectorDiff::Set { index: 5, value } => value);
    let event_5_item = item_5.as_event().unwrap();
    assert_eq!(event_5_item.event_id(), Some(event_5_id));
    let profile =
        assert_matches!(event_5_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    assert_pending!(timeline_stream);
}
