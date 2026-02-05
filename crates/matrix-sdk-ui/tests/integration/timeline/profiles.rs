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

use std::{sync::Arc, time::Duration};

use assert_matches::assert_matches;
use matrix_sdk::{
    config::{SyncSettings, SyncToken},
    test_utils::logged_in_client_with_server,
};
use matrix_sdk_common::executor::spawn;
use matrix_sdk_test::{
    ALICE, BOB, CAROL, DEFAULT_TEST_ROOM_ID, JoinedRoomBuilder, SyncResponseBuilder, async_test,
    event_factory::{EventFactory, PreviousMembership},
    mocks::mock_encryption_state,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineDetails};
use ruma::events::room::member::MembershipState;
use serde_json::json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path_regex},
};

use crate::mock_sync;

#[async_test]
async fn test_user_profile_after_being_banned() {
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);

    let f = EventFactory::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());

    // Previous content of Alice's leave event
    let prev_content_alice = PreviousMembership::new(MembershipState::Join)
        .display_name("*profanities*")
        .avatar_url("mxc://456".into());

    // Same, but for Carol.
    let prev_content_carol = PreviousMembership::new(MembershipState::Join)
        .display_name("Carol")
        .avatar_url("mxc://789".into());

    // Build a simple timeline with Bob creating the room, Alice accepting an invite
    // and sending a message, Bob sending a message, and Alice leaving
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
            // Bob create the room
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
                f.member(&ALICE).membership(MembershipState::Ban).previous(prev_content_alice),
            )
            // Carol accepts an invite into the room
            .add_timeline_event(
                f.member(&CAROL)
                    .display_name("Carol")
                    .avatar_url("mxc://789".into())
                    .membership(MembershipState::Join)
                    .previous(MembershipState::Invite),
            )
            // Carol sends a text message
            .add_timeline_event(f.text_msg("hi!").sender(&CAROL))
            // Carol leaves the room
            .add_timeline_event(
                f.member(&CAROL).membership(MembershipState::Leave).previous(prev_content_carol),
            ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    let timeline_items = timeline.items().await;
    // Date divider + 9 events
    assert_eq!(timeline_items.len(), 10);

    // Bob is still in the room, his profile should be available.
    let bob_profile = timeline_items[3].as_event().unwrap().sender_profile();
    match bob_profile {
        TimelineDetails::Ready(profile) => {
            assert_eq!(profile.display_name, Some("Bob".to_owned()));
            assert_eq!(profile.avatar_url, Some("mxc://123".into()));
        }
        _ => panic!("Expected Bob's profile to be ready"),
    }

    // Carol has left the room, but her profile should still be available.
    let carol_profile = timeline_items[8].as_event().unwrap().sender_profile();
    match carol_profile {
        TimelineDetails::Ready(profile) => {
            assert_eq!(profile.display_name, Some("Carol".to_owned()));
            assert_eq!(profile.avatar_url, Some("mxc://789".into()));
        }
        _ => panic!("Expected Carol's profile to be ready"),
    }

    // Alice's profile, and therefore display name, should be empty,
    // as she has been banned. The SDK **should** return an empty
    // profile for banned users.
    let alice_profile = timeline_items[4].as_event().unwrap().sender_profile();
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
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);

    let f = EventFactory::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());

    // Previous content of Alice's leave event
    let prev_content = PreviousMembership::new(MembershipState::Join)
        .display_name("Alice")
        .avatar_url("mxc://...".into());

    // Build a simple timeline with Bob creating the room, Alice accepting an invite
    // and sending a message, Bob sending a message, and Alice leaving
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
            // Bob create the room
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
                f.member(&ALICE).membership(MembershipState::Leave).previous(prev_content),
            ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    let timeline_items = timeline.items().await;
    // Date divider + 5 events
    assert_eq!(timeline_items.len(), 6);

    // Alice's profile, and therefore display name, should be still available after
    // she left the room.
    let alice_profile = timeline_items[4].as_event().unwrap().sender_profile();
    match alice_profile {
        TimelineDetails::Ready(profile) => {
            assert_eq!(profile.display_name, Some("Alice".to_owned()));
            assert_eq!(profile.avatar_url, Some("mxc://...".into()));
        }
        _ => panic!("Expected Alice's profile to be ready"),
    }

    // Bob's profile should also be available
    let bob_profile = timeline_items[3].as_event().unwrap().sender_profile();
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
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);

    let f = EventFactory::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    let timeline = Arc::new(room.timeline().await.unwrap());

    sync_builder.add_joined_room(
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
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

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
    Mock::given(method("GET"))
        .and(path_regex(r"/_matrix/client/r0/rooms/.*/members"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": [
                // This event wasn't previously observed by the client
                f.member(&CAROL).membership(MembershipState::Join).into_raw_timeline(),

                // Same as above
                f.member(&ALICE).membership(MembershipState::Join).previous(MembershipState::Invite).into_raw_timeline(),

                f.member(&CAROL).kicked(&BOB).previous(MembershipState::Join).into_raw_timeline(),
            ]
        })))
        .mount(&server)
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
