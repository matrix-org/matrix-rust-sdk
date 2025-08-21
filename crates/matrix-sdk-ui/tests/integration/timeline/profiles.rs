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
    event_factory::EventFactory, mocks::mock_encryption_state,
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
