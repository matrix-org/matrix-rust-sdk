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
    config::{SyncSettings, SyncToken},
    test_utils::logged_in_client_with_server,
};
use matrix_sdk_common::executor::spawn;
use matrix_sdk_test::{
    ALICE, BOB, JoinedRoomBuilder, SyncResponseBuilder, async_test, event_factory::EventFactory,
    mocks::mock_encryption_state,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineDetails};
use ruma::{
    event_id,
    events::room::{
        member::MembershipState,
        message::{MessageType, RoomMessageEventContent},
    },
    room_id, user_id,
};
use stream_assert::assert_pending;

use crate::mock_sync;

#[async_test]
async fn test_batched() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);

    let f = EventFactory::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().event_filter(|_, _| true).build().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let hdl = spawn(async move {
        let next_batch = timeline_stream.next().await.unwrap();
        // There can be more than three updates because we add things like
        // date dividers and implicit read receipts
        assert!(next_batch.len() >= 3);
    });

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("text message event").sender(&ALICE))
            .add_timeline_event(
                f.member(&BOB).membership(MembershipState::Join).previous(MembershipState::Invite),
            )
            .add_timeline_event(f.notice("notice message event").sender(&BOB)),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    tokio::time::timeout(Duration::from_millis(500), hdl).await.unwrap().unwrap();
}

#[async_test]
async fn test_event_filter() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);
    let f = EventFactory::new();

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
        f.text_msg("hello").sender(user_id!("@alice:example.org")).event_id(first_event_id),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value: first } = &timeline_updates[0]);
    let first_event = first.as_event().unwrap();
    assert_eq!(first_event.event_id(), Some(first_event_id));
    assert_eq!(first_event.read_receipts().len(), 1, "implicit read receipt");
    assert_matches!(first_event.latest_edit_json(), None);
    assert_let!(Some(msg) = first_event.content().as_message());
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert!(!msg.is_edited());

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    let second_event_id = event_id!("$Ga6Y2l0gKY");
    let edit_event_id = event_id!("$7i9In0gEmB");
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_bulk([
            f.text_html("Test", "<em>Test</em>")
                .sender(user_id!("@bob:example.org"))
                .event_id(second_event_id)
                .into(),
            f.text_msg(" * hi")
                .sender(user_id!("@alice:example.org"))
                .event_id(edit_event_id)
                .edit(first_event_id, RoomMessageEventContent::text_plain("hi").into())
                .into(),
        ]),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 3);

    assert_let!(VectorDiff::PushBack { value: second } = &timeline_updates[0]);
    let second_event = second.as_event().unwrap();
    assert_eq!(second_event.event_id(), Some(second_event_id));
    assert_eq!(second_event.read_receipts().len(), 1, "implicit read receipt");

    // The implicit read receipt of Alice is moving from Alice's message...
    assert_let!(VectorDiff::Set { index: 1, value: first } = &timeline_updates[1]);
    assert_eq!(first.as_event().unwrap().read_receipts().len(), 0, "no more implicit read receipt");
    // … to Alice's edit. But since this item isn't visible, it's lost in the weeds!

    // The edit is applied to the first event.
    assert_let!(VectorDiff::Set { index: 1, value: first } = &timeline_updates[2]);
    let first_event = first.as_event().unwrap();
    assert!(first_event.read_receipts().is_empty());
    assert_matches!(first_event.latest_edit_json(), Some(_));
    assert_let!(Some(msg) = first_event.content().as_message());
    assert_let!(MessageType::Text(text) = msg.msgtype());
    assert_eq!(text.body, "hi");
    assert!(msg.is_edited());

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_timeline_is_reset_when_a_user_is_ignored_or_unignored() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().build().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

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

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 5);

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(first_event_id));

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[1]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(second_event_id));

    assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[2]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(first_event_id));

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[3]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(third_event_id));

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[4]);
    assert!(value.is_date_divider());

    assert_pending!(timeline_stream);

    sync_builder.add_global_account_data(ev_factory.ignored_user_list([bob.to_owned()]));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // The timeline has been emptied.
    assert_let!(VectorDiff::Clear = &timeline_updates[0]);

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

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 4);

    // Timeline receives events as before.
    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(fourth_event_id));

    assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[1]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(fourth_event_id));

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[2]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(fifth_event_id));

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[3]);
    assert!(value.is_date_divider());

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_profile_updates() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);
    let f = EventFactory::new();

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline_builder().build().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let alice = user_id!("@alice:example.org");
    let bob = user_id!("@bob:example.org");

    // Add users with unknown profile.
    let event_1_id = event_id!("$YTQwYl2pl1");
    let event_2_id = event_id!("$YTQwYl2pl2");

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_bulk([
        f.text_msg("hello").event_id(event_1_id).sender(alice).into(),
        f.text_msg("hello").event_id(event_2_id).sender(bob).into(),
    ]));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 3);

    assert_let!(VectorDiff::PushBack { value: item_1 } = &timeline_updates[0]);
    let event_1_item = item_1.as_event().unwrap();
    assert_eq!(event_1_item.event_id(), Some(event_1_id));
    assert_matches!(event_1_item.sender_profile(), TimelineDetails::Unavailable);

    assert_let!(VectorDiff::PushBack { value: item_2 } = &timeline_updates[1]);
    let event_2_item = item_2.as_event().unwrap();
    assert_eq!(event_2_item.event_id(), Some(event_2_id));
    assert_matches!(event_2_item.sender_profile(), TimelineDetails::Unavailable);

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[2]);
    assert!(value.is_date_divider());

    assert_pending!(timeline_stream);

    // Add profiles of users and other message.
    let event_3_id = event_id!("$YTQwYl2pl3");
    let event_4_id = event_id!("$YTQwYl2pl4");
    let event_5_id = event_id!("$YTQwYl2pl5");

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_bulk([
        f.member(bob).display_name("Member").event_id(event_3_id).into(),
        f.member(alice).display_name("Alice").event_id(event_4_id).into(),
        f.text_msg("hello").event_id(event_5_id).sender(alice).into(),
    ]));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 8);

    // Read receipt change.
    assert_let!(VectorDiff::Set { index: 2, value } = &timeline_updates[0]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(event_2_id));

    // The events are added.
    assert_let!(VectorDiff::PushBack { value: item_3 } = &timeline_updates[1]);
    let event_3_item = item_3.as_event().unwrap();
    assert_eq!(event_3_item.event_id(), Some(event_3_id));
    let profile =
        assert_matches!(event_3_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(!profile.display_name_ambiguous);

    // Read receipt change.
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[2]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(event_1_id));

    assert_let!(VectorDiff::PushBack { value: item_4 } = &timeline_updates[3]);
    let event_4_item = item_4.as_event().unwrap();
    assert_eq!(event_4_item.event_id(), Some(event_4_id));
    let profile =
        assert_matches!(event_4_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Alice"));
    assert!(!profile.display_name_ambiguous);

    // Read receipt change.
    assert_let!(VectorDiff::Set { index: 4, value } = &timeline_updates[4]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(event_4_id));

    assert_let!(VectorDiff::PushBack { value: item_5 } = &timeline_updates[5]);
    let event_5_item = item_5.as_event().unwrap();
    assert_eq!(event_5_item.event_id(), Some(event_5_id));
    let profile =
        assert_matches!(event_5_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Alice"));
    assert!(!profile.display_name_ambiguous);

    // The profiles changed.
    assert_let!(VectorDiff::Set { index: 1, value: item_1 } = &timeline_updates[6]);
    let event_1_item = item_1.as_event().unwrap();
    assert_eq!(event_1_item.event_id(), Some(event_1_id));
    let profile =
        assert_matches!(event_1_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Alice"));
    assert!(!profile.display_name_ambiguous);

    assert_let!(VectorDiff::Set { index: 2, value: item_2 } = &timeline_updates[7]);
    let event_2_item = item_2.as_event().unwrap();
    assert_eq!(event_2_item.event_id(), Some(event_2_id));
    let profile =
        assert_matches!(event_2_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(!profile.display_name_ambiguous);

    // Change name to be ambiguous.
    let event_6_id = event_id!("$YTQwYl2pl6");

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.member(alice).display_name("Member").event_id(event_6_id)),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 7);

    // Read receipt change.
    assert_let!(VectorDiff::Set { index: 5, value } = &timeline_updates[0]);
    assert_eq!(value.as_event().unwrap().event_id(), Some(event_5_id));

    // The event is added.
    assert_let!(VectorDiff::PushBack { value: item_6 } = &timeline_updates[1]);
    let event_6_item = item_6.as_event().unwrap();
    assert_eq!(event_6_item.event_id(), Some(event_6_id));
    let profile =
        assert_matches!(event_6_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    // The profiles changed.
    assert_let!(VectorDiff::Set { index: 1, value: item_1 } = &timeline_updates[2]);
    let event_1_item = item_1.as_event().unwrap();
    assert_eq!(event_1_item.event_id(), Some(event_1_id));
    let profile =
        assert_matches!(event_1_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    assert_let!(VectorDiff::Set { index: 2, value: item_2 } = &timeline_updates[3]);
    let event_2_item = item_2.as_event().unwrap();
    assert_eq!(event_2_item.event_id(), Some(event_2_id));
    let profile =
        assert_matches!(event_2_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    assert_let!(VectorDiff::Set { index: 3, value: item_3 } = &timeline_updates[4]);
    let event_3_item = item_3.as_event().unwrap();
    assert_eq!(event_3_item.event_id(), Some(event_3_id));
    let profile =
        assert_matches!(event_3_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    assert_let!(VectorDiff::Set { index: 4, value: item_4 } = &timeline_updates[5]);
    let event_4_item = item_4.as_event().unwrap();
    assert_eq!(event_4_item.event_id(), Some(event_4_id));
    let profile =
        assert_matches!(event_4_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    assert_let!(VectorDiff::Set { index: 5, value: item_5 } = &timeline_updates[6]);
    let event_5_item = item_5.as_event().unwrap();
    assert_eq!(event_5_item.event_id(), Some(event_5_id));
    let profile =
        assert_matches!(event_5_item.sender_profile(), TimelineDetails::Ready(profile) => profile);
    assert_eq!(profile.display_name.as_deref(), Some("Member"));
    assert!(profile.display_name_ambiguous);

    assert_pending!(timeline_stream);
}
