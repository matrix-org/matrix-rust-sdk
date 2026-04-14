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

use std::sync::Arc;

use assert_matches::assert_matches;
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_common::executor::spawn;
use matrix_sdk_test::{
    ALICE, BOB, CAROL, DEFAULT_TEST_ROOM_ID, JoinedRoomBuilder, async_test,
    event_factory::{EventFactory, PreviousMembership},
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineDetails};
use ruma::events::room::member::MembershipState;

#[async_test]
async fn test_user_profile_after_being_banned() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let f = EventFactory::new();

    server.mock_room_state_encryption().plain().mount().await;

    let room = server.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;
    let timeline = Arc::new(room.timeline().await.unwrap());

    // Build a simple timeline with Bob joining the room, Alice accepting an invite,
    // sending some messages, and then getting banned by Bob. Alice's profile should
    // be unavailable after the ban, while Bob's profile should be unaffected.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
                // Bob joins the room
                .add_timeline_event(
                    f.member(&BOB)
                        .display_name("Bob")
                        .avatar_url("mxc://123".into())
                        .membership(MembershipState::Join),
                )
                // Alice accepts an invite into the room
                .add_timeline_event(
                    f.member(&ALICE)
                        .display_name("*profanities*")
                        .avatar_url("mxc://456".into())
                        .membership(MembershipState::Join)
                        .previous(MembershipState::Invite),
                )
                // Bob sends a text message
                .add_timeline_event(f.text_msg("hey!").sender(&BOB))
                // Alice send some insults and gets banned
                .add_timeline_event(f.text_msg("*insults*").sender(&ALICE))
                .add_timeline_event(f.text_msg("Nope. You are out.").sender(&BOB))
                .add_timeline_event(
                    f.member(&ALICE).membership(MembershipState::Ban).sender(&BOB).previous(
                        PreviousMembership::new(MembershipState::Join)
                            .display_name("*profanities*")
                            .avatar_url("mxc://456".into()),
                    ),
                ),
        )
        .await;

    let timeline_items = timeline.items().await;
    // Date divider + 6 events
    assert_eq!(timeline_items.len(), 7);

    // Alice's profile, and therefore display name, should be empty,
    // as she has been banned. The SDK **should** return an empty
    // profile for banned users.
    // We are only checking Alice's profile here, as we check Bob's profile in a
    // separate test
    let alice_event = timeline_items[4].as_event().unwrap();
    let alice_profile = alice_event.sender_profile();
    // Was this event sent by Alice?
    assert_eq!(alice_event.sender(), *ALICE);
    match alice_profile {
        TimelineDetails::Ready(profile) => {
            assert_eq!(profile.display_name, None);
            assert_eq!(profile.avatar_url, None);
        }
        _ => panic!("Expected Alice's profile to be ready"),
    }
}

#[async_test]
async fn test_user_profile_after_leaving() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let f = EventFactory::new();

    server.mock_room_state_encryption().plain().mount().await;

    let room = server.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;
    let timeline = Arc::new(room.timeline().await.unwrap());

    // Build a simple timeline with Bob joining the room, Alice accepting an invite
    // and sending a message, Bob sending a message, and Alice leaving
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
                // Bob joins the room
                .add_timeline_event(
                    f.member(&BOB)
                        .display_name("Bob")
                        .avatar_url("mxc://...".into())
                        .membership(MembershipState::Join),
                )
                // Alice accepts an invite into the room
                .add_timeline_event(
                    f.member(&ALICE)
                        .display_name("Alice")
                        .avatar_url("mxc://...".into())
                        .membership(MembershipState::Join)
                        .previous(MembershipState::Invite),
                )
                // Bob sends a text message
                .add_timeline_event(f.text_msg("hey!").sender(&BOB))
                // Alice send a message and leaves the room
                .add_timeline_event(f.text_msg("bye now").sender(&ALICE))
                .add_timeline_event(
                    f.member(&ALICE).membership(MembershipState::Leave).previous(
                        PreviousMembership::new(MembershipState::Join)
                            .display_name("Alice")
                            .avatar_url("mxc://...".into()),
                    ),
                ),
        )
        .await;

    let timeline_items = timeline.items().await;
    // Date divider + 5 events
    assert_eq!(timeline_items.len(), 6);

    // Alice's profile, and therefore display name, should be still available after
    // she left the room.
    let alice_event = timeline_items[4].as_event().unwrap();
    let alice_profile = alice_event.sender_profile();
    // Was this event sent by Alice?
    assert_eq!(alice_event.sender(), *ALICE);
    match alice_profile {
        TimelineDetails::Ready(profile) => {
            assert_eq!(profile.display_name, Some("Alice".to_owned()));
            assert_eq!(profile.avatar_url, Some("mxc://...".into()));
        }
        _ => panic!("Expected Alice's profile to be ready"),
    }

    // Bob's profile should also be available
    let bob_event = timeline_items[3].as_event().unwrap();
    let bob_profile = bob_event.sender_profile();
    // Was this event sent by Bob?
    assert_eq!(bob_event.sender(), *BOB);
    match bob_profile {
        TimelineDetails::Ready(profile) => {
            assert_eq!(profile.display_name, Some("Bob".to_owned()));
            assert_eq!(profile.avatar_url, Some("mxc://...".into()));
        }
        _ => panic!("Expected Bob's profile to be ready"),
    }
}

#[async_test]
async fn test_update_sender_profiles() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let f = EventFactory::new();

    server.mock_room_state_encryption().plain().mount().await;

    let room = server.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;
    let timeline = Arc::new(room.timeline().await.unwrap());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
                // Alice accepts an invite into the room
                .add_timeline_event(
                    f.member(&ALICE)
                        .membership(MembershipState::Join)
                        .previous(MembershipState::Invite),
                )
                // Bob sends a text message
                .add_timeline_event(f.text_msg("text message event").sender(&BOB))
                // Carol kicks bob
                .add_timeline_event(f.member(&CAROL).kicked(&BOB).previous(MembershipState::Join)),
        )
        .await;

    let timeline_items = timeline.items().await;
    assert_eq!(timeline_items.len(), 4);
    assert!(timeline_items[0].is_date_divider());
    // We have seen a member event for alice from alice => we have a profile
    // albeit empty.
    assert_matches!(
        timeline_items[1].as_event().unwrap().sender_profile(),
        TimelineDetails::Ready(_)
    );
    // We haven't seen a member event for bob from himself.
    assert_matches!(
        timeline_items[2].as_event().unwrap().sender_profile(),
        TimelineDetails::Ready(_) // ?
    );
    // We haven't seen a member event from carol at all.
    assert_matches!(
        timeline_items[3].as_event().unwrap().sender_profile(),
        TimelineDetails::Unavailable
    );

    // Try to get the missing profiles
    timeline.fetch_members().await;

    // No HTTP mock installed, unavailable profiles are now in error state.

    let timeline_items = timeline.items().await;
    assert_eq!(timeline_items.len(), 4);
    assert!(timeline_items[0].is_date_divider());
    assert_matches!(
        timeline_items[1].as_event().unwrap().sender_profile(),
        TimelineDetails::Ready(_)
    );
    assert_matches!(
        timeline_items[2].as_event().unwrap().sender_profile(),
        TimelineDetails::Ready(_)
    );
    assert_matches!(
        timeline_items[3].as_event().unwrap().sender_profile(),
        TimelineDetails::Error(_)
    );

    let f = f.room(&DEFAULT_TEST_ROOM_ID);

    // Try again, this time we mock the endpoint to get a useful response.
    server
        .mock_get_members()
        .ok(vec![
            // This event wasn't previously observed by the client
            f.member(&CAROL).membership(MembershipState::Join).into_raw(),
            // Same as above
            f.member(&ALICE)
                .membership(MembershipState::Join)
                .previous(MembershipState::Invite)
                .into_raw(),
            f.member(&CAROL).kicked(&BOB).previous(MembershipState::Join).into_raw(),
        ])
        .mount()
        .await;

    // Spawn fetch_members as a background task, so we can observe the missing
    // profiles being set to pending first.
    let hdl = spawn({
        let timeline = timeline.clone();
        async move {
            timeline.fetch_members().await;
        }
    });

    tokio::task::yield_now().await;

    let timeline_items = timeline.items().await;
    assert_eq!(timeline_items.len(), 4);
    assert!(timeline_items[0].is_date_divider());
    assert_matches!(
        timeline_items[1].as_event().unwrap().sender_profile(),
        TimelineDetails::Ready(_)
    );
    assert_matches!(
        timeline_items[2].as_event().unwrap().sender_profile(),
        TimelineDetails::Ready(_)
    );
    assert_matches!(
        timeline_items[3].as_event().unwrap().sender_profile(),
        TimelineDetails::Pending
    );

    // Wait for fetching members to be done.
    hdl.await.unwrap();

    let timeline_items = timeline.items().await;
    assert_eq!(timeline_items.len(), 4);
    assert!(timeline_items[0].is_date_divider());
    assert_matches!(
        timeline_items[1].as_event().unwrap().sender_profile(),
        TimelineDetails::Ready(_)
    );
    assert_matches!(
        timeline_items[2].as_event().unwrap().sender_profile(),
        TimelineDetails::Ready(_)
    );
    // Carol's profile is now available
    assert_matches!(
        timeline_items[3].as_event().unwrap().sender_profile(),
        TimelineDetails::Ready(_)
    );
}
