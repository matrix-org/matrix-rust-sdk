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
// See the License for that specific language governing permissions and
// limitations under the License.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use assert_matches::assert_matches;
use matrix_sdk::{
    assert_next_matches_with_timeout,
    test_utils::{logged_in_client_with_server, mocks::MatrixMockServer},
};
use matrix_sdk_test::async_test;
use matrix_sdk_ui::sync_service::{State, SyncService};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use wiremock::{Match as _, Mock, MockGuard, MockServer, Request, ResponseTemplate};

use crate::sliding_sync::{PartialSlidingSyncRequest, SlidingSyncMatcher};

/// Sets up a sliding sync server that use different `pos` values for the
/// encrptyion and the room sync.
async fn setup_mocking_sliding_sync_server(
    server: &MockServer,
    encryption_pos: Arc<Mutex<i32>>,
    room_pos: Arc<Mutex<i32>>,
) -> MockGuard {
    Mock::given(SlidingSyncMatcher)
        .respond_with(move |request: &Request| {
            let partial_request: PartialSlidingSyncRequest = request.body_json().unwrap();
            // Repeat the transaction id in the response, to validate sticky parameters.
            let mut pos = match partial_request.conn_id.as_deref() {
                Some("encryption") => encryption_pos.lock().unwrap(),
                Some("room-list") => room_pos.lock().unwrap(),
                _ => panic!("unexpected conn id {:?}", partial_request.conn_id),
            };

            *pos += 1;
            let pos_as_str = (*pos).to_string();

            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "txn_id": partial_request.txn_id,
                    "pos": pos_as_str
                }))
                .set_delay(Duration::from_millis(50))
        })
        .mount_as_scoped(server)
        .await
}

#[async_test]
async fn test_sync_service_state() -> anyhow::Result<()> {
    let (client, server) = logged_in_client_with_server().await;

    let encryption_pos = Arc::new(Mutex::new(0));
    let room_pos = Arc::new(Mutex::new(0));
    let guard =
        setup_mocking_sliding_sync_server(&server, encryption_pos.clone(), room_pos.clone()).await;

    let sync_service = SyncService::builder(client).build().await.unwrap();

    let mut state_stream = sync_service.state();

    // At first, the sync service is sleeping.
    assert_matches!(state_stream.get(), State::Idle);
    assert!(server.received_requests().await.unwrap().is_empty());
    assert!(!sync_service.is_supervisor_running().await);
    assert!(sync_service.try_get_encryption_sync_permit().is_some());

    // After starting, the sync service is, well, running.
    sync_service.start().await;
    assert_next_matches!(state_stream, State::Running);
    assert!(sync_service.is_supervisor_running().await);
    assert!(sync_service.try_get_encryption_sync_permit().is_none());

    // Restarting while started doesn't change the current state.
    sync_service.start().await;
    assert_pending!(state_stream);
    assert!(sync_service.is_supervisor_running().await);
    assert!(sync_service.try_get_encryption_sync_permit().is_none());

    // Let the server respond a few times.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Pausing will stop both syncs, after a bit of delay.
    sync_service.stop().await;
    assert_next_matches!(state_stream, State::Idle);
    assert!(!sync_service.is_supervisor_running().await);
    assert!(sync_service.try_get_encryption_sync_permit().is_some());

    let mut num_encryption_sync_requests: i32 = 0;
    let mut num_room_list_requests = 0;
    let mut latest_room_list_pos = None;
    for request in &server.received_requests().await.expect("Request recording has been disabled") {
        if !SlidingSyncMatcher.matches(request) {
            continue;
        }

        let mut json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

        if let Some(root) = json_value.as_object_mut()
            && let Some(conn_id) = root.get("conn_id").and_then(|obj| obj.as_str())
        {
            if conn_id == "encryption" {
                num_encryption_sync_requests += 1;
            } else if conn_id == "room-list" {
                num_room_list_requests += 1;

                // Retrieve the position used in the query.
                for (key, val) in request.url.query_pairs() {
                    if key == "pos" {
                        latest_room_list_pos = Some(val.to_string());
                    }
                }
            } else {
                panic!("unexpected conn id seen server side: {conn_id}");
            }
        }
    }

    assert!(num_encryption_sync_requests > 0);
    assert!(num_room_list_requests > 0);
    assert!(
        (num_encryption_sync_requests - num_room_list_requests).abs() <= 1,
        "encryption:{num_encryption_sync_requests} / room_list:{num_room_list_requests}"
    );
    assert!(latest_room_list_pos.is_some());

    // Now reset the server, wait for a bit that it doesn't receive extra requests.
    drop(guard);
    server.reset().await;
    let _guard =
        setup_mocking_sliding_sync_server(&server, encryption_pos.clone(), room_pos.clone()).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(server.received_requests().await.unwrap().is_empty());

    // When restarting and waiting a bit, the server gets new requests, starting at
    // the same position than just before being stopped.
    sync_service.start().await;
    assert_next_matches!(state_stream, State::Running);
    assert!(sync_service.is_supervisor_running().await);
    assert!(sync_service.try_get_encryption_sync_permit().is_none());

    tokio::time::sleep(Duration::from_millis(100)).await;

    num_encryption_sync_requests = 0;
    num_room_list_requests = 0;

    for request in &server.received_requests().await.expect("Request recording has been disabled") {
        if !SlidingSyncMatcher.matches(request) {
            continue;
        }

        let mut json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

        if let Some(root) = json_value.as_object_mut()
            && let Some(conn_id) = root.get("conn_id").and_then(|obj| obj.as_str())
        {
            if conn_id == "encryption" {
                num_encryption_sync_requests += 1;
            } else if conn_id == "room-list" {
                if num_room_list_requests == 0 {
                    // Either it's the same pos, or it's the next one if the request could be
                    // processed by the client.
                    let mut current_pos = None;
                    for (key, val) in request.url.query_pairs() {
                        if key == "pos" {
                            current_pos = Some(val);
                        }
                    }
                    let current_pos: i32 = current_pos.unwrap().parse()?;
                    let prev_pos: i32 = latest_room_list_pos.take().unwrap().parse()?;
                    assert!((current_pos - prev_pos).abs() <= 1);
                }

                num_room_list_requests += 1;
            } else {
                panic!("unexpected conn id seen server side: {conn_id}");
            }
        }
    }

    assert!(num_encryption_sync_requests > 0);
    assert!(num_room_list_requests > 0);
    assert!(
        (num_encryption_sync_requests - num_room_list_requests).abs() <= 1,
        "encryption:{num_encryption_sync_requests} / room_list:{num_room_list_requests}"
    );

    assert_pending!(state_stream);

    Ok(())
}

#[async_test]
async fn test_sync_service_offline_mode() {
    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    let sync_service = SyncService::builder(client).with_offline_mode().build().await.unwrap();
    let mut states = sync_service.state();

    Mock::given(SlidingSyncMatcher)
        .respond_with(ResponseTemplate::new(404))
        .expect(1..)
        .mount(mock_server.server())
        .await;

    {
        let _versions_guard = mock_server.mock_versions().error500().mount_as_scoped().await;

        sync_service.start().await;
        assert_next_matches!(states, State::Running);
        assert_next_matches_with_timeout!(states, 2000, State::Offline);
    }

    mock_server.mock_versions().ok().expect(1..).mount().await;

    assert_next_matches_with_timeout!(states, 1000, State::Running);
}

#[async_test]
async fn test_sync_service_offline_mode_stopping() {
    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    let sync_service = SyncService::builder(client).with_offline_mode().build().await.unwrap();
    let mut states = sync_service.state();

    Mock::given(SlidingSyncMatcher)
        .respond_with(ResponseTemplate::new(404))
        .expect(1..)
        .mount(mock_server.server())
        .await;
    mock_server.mock_versions().error500().mount().await;

    sync_service.start().await;
    assert_next_matches!(states, State::Running);

    assert_next_matches_with_timeout!(states, 2000, State::Offline);
    sync_service.stop().await;
    assert_next_matches_with_timeout!(states, 2000, State::Idle);
}

#[async_test]
async fn test_sync_service_offline_mode_restarting() {
    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    let sync_service = SyncService::builder(client).with_offline_mode().build().await.unwrap();
    let mut states = sync_service.state();

    Mock::given(SlidingSyncMatcher)
        .respond_with(ResponseTemplate::new(404))
        .mount(mock_server.server())
        .await;
    mock_server.mock_versions().error500().mount().await;

    sync_service.start().await;
    assert_next_matches!(states, State::Running);
    assert_next_matches_with_timeout!(states, 2000, State::Offline);

    sync_service.start().await;

    assert_next_matches_with_timeout!(states, 2000, State::Running);
    assert_next_matches_with_timeout!(states, 2000, State::Offline);
}
