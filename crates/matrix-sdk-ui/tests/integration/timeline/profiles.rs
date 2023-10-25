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
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{
    async_test, EventBuilder, JoinedRoomBuilder, SyncResponseBuilder, ALICE, BOB, CAROL,
    DEFAULT_TEST_ROOM_ID,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineDetails};
use ruma::events::room::{
    member::{MembershipState, RoomMemberEventContent},
    message::RoomMessageEventContent,
};
use serde_json::json;
use wiremock::{
    matchers::{method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync};

#[async_test]
async fn update_sender_profiles() {
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    let timeline = Arc::new(room.timeline().await);

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
            // Alice accepts an invite into the room
            .add_timeline_event(event_builder.make_sync_state_event(
                &ALICE,
                ALICE.as_str(),
                RoomMemberEventContent::new(MembershipState::Join),
                Some(RoomMemberEventContent::new(MembershipState::Invite)),
            ))
            // Bob sends a text message
            .add_timeline_event(event_builder.make_sync_message_event(
                &BOB,
                RoomMessageEventContent::text_plain("text message event"),
            ))
            // Carol kicks bob
            .add_timeline_event(event_builder.make_sync_state_event(
                &CAROL,
                BOB.as_str(),
                RoomMemberEventContent::new(MembershipState::Leave),
                Some(RoomMemberEventContent::new(MembershipState::Join)),
            )),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    let timeline_items = timeline.items().await;
    assert_eq!(timeline_items.len(), 4);
    assert!(timeline_items[0].is_day_divider());
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
    assert!(timeline_items[0].is_day_divider());
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

    // Try again, this time we mock the endpoint to get a useful response.
    Mock::given(method("GET"))
        .and(path_regex(r"/_matrix/client/r0/rooms/.*/members"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": [
                // This event wasn't previously observed by the client
                event_builder.make_state_event(
                    &CAROL,
                    &DEFAULT_TEST_ROOM_ID,
                    CAROL.as_str(),
                    RoomMemberEventContent::new(MembershipState::Join),
                    None,
                ),

                // Same as above
                event_builder.make_state_event(
                    &ALICE,
                    &DEFAULT_TEST_ROOM_ID,
                    ALICE.as_str(),
                    RoomMemberEventContent::new(MembershipState::Join),
                    Some(RoomMemberEventContent::new(MembershipState::Invite)),
                ),

                event_builder.make_state_event(
                    &CAROL,
                    &DEFAULT_TEST_ROOM_ID,
                    BOB.as_str(),
                    RoomMemberEventContent::new(MembershipState::Leave),
                    Some(RoomMemberEventContent::new(MembershipState::Join)),
                ),
            ]
        })))
        .mount(&server)
        .await;

    // Spawn fetch_members as a background task, so we can observe the missing
    // profiles being set to pending first.
    let hdl = tokio::spawn({
        let timeline = timeline.clone();
        async move {
            timeline.fetch_members().await;
        }
    });

    tokio::task::yield_now().await;

    let timeline_items = timeline.items().await;
    assert_eq!(timeline_items.len(), 4);
    assert!(timeline_items[0].is_day_divider());
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
    assert!(timeline_items[0].is_day_divider());
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
