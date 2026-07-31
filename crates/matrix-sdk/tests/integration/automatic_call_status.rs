// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use matrix_sdk::{
    Client,
    ruma::{
        DeviceId, UserId,
        api::MatrixVersion,
        events::{
            AnySyncStateEvent,
            call::member::{CallMemberEventContent, CallMemberStateKey},
        },
        profile::ProfileFieldName,
        serde::Raw,
    },
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::room_id;

/// A join-shaped `m.call.member` state event for `(user_id, device_id)`.
fn join_call_event(user_id: &UserId, device_id: &DeviceId) -> Raw<AnySyncStateEvent> {
    EventFactory::new()
        .call_membership_state(user_id.to_owned(), device_id.to_string())
        .into_raw_sync_state()
}

/// A leave-shaped `m.call.member` state event for `(user_id, device_id)`.
fn leave_call_event(user_id: &UserId, device_id: &DeviceId) -> Raw<AnySyncStateEvent> {
    let state_key = CallMemberStateKey::new(user_id.to_owned(), Some(device_id.to_string()), true);
    EventFactory::new()
        .event(CallMemberEventContent::new_empty(None))
        .sender(user_id)
        .state_key(state_key.as_ref())
        .into_raw_sync_state()
}

async fn make_client() -> (MatrixMockServer, Client) {
    let mock_server = MatrixMockServer::new().await;
    // V1_16 is required so the SDK picks the stable
    // `/v3/profile/{user}/{field}` endpoint path (which is what
    // `mock_set_profile_field` intercepts) rather than the unstable
    // `/unstable/uk.tcpip.msc4133/profile/...` variant.
    let client =
        mock_server.client_builder().server_versions(vec![MatrixVersion::V1_16]).build().await;
    (mock_server, client)
}

#[async_test]
async fn test_auto_sync_sets_m_call_when_own_device_joins_a_room_call() {
    let (mock_server, client) = make_client().await;
    let own_user_id = client.user_id().unwrap().to_owned();
    let own_device_id = client.device_id().unwrap().to_owned();

    client.enable_automatic_call_status(true);

    let _put = mock_server
        .mock_set_profile_field(&own_user_id, ProfileFieldName::Call)
        .ok()
        .mock_once()
        .named("PUT on join")
        .mount_as_scoped()
        .await;

    let room_id = room_id!("!room:example.org");
    let joined = JoinedRoomBuilder::new(room_id)
        .add_state_event(join_call_event(&own_user_id, &own_device_id));
    mock_server.sync_room(&client, joined).await;
}

#[async_test]
async fn test_auto_sync_clears_m_call_when_own_device_leaves_the_room_call() {
    let (mock_server, client) = make_client().await;
    let own_user_id = client.user_id().unwrap().to_owned();
    let own_device_id = client.device_id().unwrap().to_owned();

    client.enable_automatic_call_status(true);

    let room_id = room_id!("!room:example.org");

    {
        let _put = mock_server
            .mock_set_profile_field(&own_user_id, ProfileFieldName::Call)
            .ok()
            .mock_once()
            .named("PUT on join")
            .mount_as_scoped()
            .await;

        let joined = JoinedRoomBuilder::new(room_id)
            .add_state_event(join_call_event(&own_user_id, &own_device_id));
        mock_server.sync_room(&client, joined).await;
    }

    {
        let _delete = mock_server
            .mock_delete_profile_field(&own_user_id, ProfileFieldName::Call)
            .ok()
            .mock_once()
            .named("DELETE on leave")
            .mount_as_scoped()
            .await;

        let leaving = JoinedRoomBuilder::new(room_id)
            .add_state_event(leave_call_event(&own_user_id, &own_device_id));
        mock_server.sync_room(&client, leaving).await;
    }
}

#[async_test]
async fn test_auto_sync_does_nothing_when_disabled() {
    let (mock_server, client) = make_client().await;
    let own_user_id = client.user_id().unwrap().to_owned();
    let own_device_id = client.device_id().unwrap().to_owned();

    // Deliberately do NOT enable auto-sync.
    let _no_put = mock_server
        .mock_set_profile_field(&own_user_id, ProfileFieldName::Call)
        .ok()
        .expect(0)
        .named("no PUT while disabled")
        .mount_as_scoped()
        .await;

    let _no_delete = mock_server
        .mock_delete_profile_field(&own_user_id, ProfileFieldName::Call)
        .ok()
        .expect(0)
        .named("no DELETE while disabled")
        .mount_as_scoped()
        .await;

    let room_id = room_id!("!room:example.org");
    let joined = JoinedRoomBuilder::new(room_id)
        .add_state_event(join_call_event(&own_user_id, &own_device_id));
    mock_server.sync_room(&client, joined).await;
}

#[async_test]
async fn test_auto_sync_still_fires_delete_after_failed_put() {
    // After a failed PUT the state machine must NOT get stuck: the next
    // real transition (join → leave) should still fire. We update
    // `was_in_call` optimistically for exactly this reason.
    use wiremock::ResponseTemplate;

    let (mock_server, client) = make_client().await;
    let own_user_id = client.user_id().unwrap().to_owned();
    let own_device_id = client.device_id().unwrap().to_owned();

    client.enable_automatic_call_status(true);

    let room_id = room_id!("!room:example.org");

    {
        let _fail = mock_server
            .mock_set_profile_field(&own_user_id, ProfileFieldName::Call)
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .named("PUT fails")
            .mount_as_scoped()
            .await;

        let joined = JoinedRoomBuilder::new(room_id)
            .add_state_event(join_call_event(&own_user_id, &own_device_id));
        mock_server.sync_room(&client, joined).await;
    }

    {
        let _delete = mock_server
            .mock_delete_profile_field(&own_user_id, ProfileFieldName::Call)
            .ok()
            .mock_once()
            .named("DELETE on leave after prior PUT failed")
            .mount_as_scoped()
            .await;

        let leaving = JoinedRoomBuilder::new(room_id)
            .add_state_event(leave_call_event(&own_user_id, &own_device_id));
        mock_server.sync_room(&client, leaving).await;
    }
}

#[async_test]
async fn test_runtime_toggle_off_stops_writes_and_on_resumes_them() {
    let (mock_server, client) = make_client().await;
    let own_user_id = client.user_id().unwrap().to_owned();
    let own_device_id = client.device_id().unwrap().to_owned();

    let room_id = room_id!("!room:example.org");

    // Phase 1: enable + join → one PUT.
    client.enable_automatic_call_status(true);
    {
        let _put = mock_server
            .mock_set_profile_field(&own_user_id, ProfileFieldName::Call)
            .ok()
            .mock_once()
            .named("phase 1: PUT on join while enabled")
            .mount_as_scoped()
            .await;

        let joined = JoinedRoomBuilder::new(room_id)
            .add_state_event(join_call_event(&own_user_id, &own_device_id));
        mock_server.sync_room(&client, joined).await;
    }

    // Phase 2: disable + leave → no writes.
    client.enable_automatic_call_status(false);
    {
        let _no_put = mock_server
            .mock_set_profile_field(&own_user_id, ProfileFieldName::Call)
            .ok()
            .expect(0)
            .named("phase 2: no PUT while disabled")
            .mount_as_scoped()
            .await;
        let _no_delete = mock_server
            .mock_delete_profile_field(&own_user_id, ProfileFieldName::Call)
            .ok()
            .expect(0)
            .named("phase 2: no DELETE while disabled")
            .mount_as_scoped()
            .await;

        let leaving = JoinedRoomBuilder::new(room_id)
            .add_state_event(leave_call_event(&own_user_id, &own_device_id));
        mock_server.sync_room(&client, leaving).await;
    }

    // Phase 3: re-enable + rejoin → one PUT
    client.enable_automatic_call_status(true);
    {
        let _put = mock_server
            .mock_set_profile_field(&own_user_id, ProfileFieldName::Call)
            .ok()
            .mock_once()
            .named("phase 3: PUT on rejoin after re-enable")
            .mount_as_scoped()
            .await;

        let rejoined = JoinedRoomBuilder::new(room_id)
            .add_state_event(join_call_event(&own_user_id, &own_device_id));
        mock_server.sync_room(&client, rejoined).await;
    }
}
