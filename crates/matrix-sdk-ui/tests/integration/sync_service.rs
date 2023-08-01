use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use matrix_sdk_test::async_test;
use matrix_sdk_ui::sync_service::{SyncService, SyncServiceState};
use serde_json::json;
use wiremock::{Match as _, Mock, MockGuard, MockServer, Request, ResponseTemplate};

use crate::{
    logged_in_client,
    sliding_sync::{PartialSlidingSyncRequest, SlidingSyncMatcher},
};

/// Sets up a sliding sync server that use different `pos` values for the
/// encrptyion and the room sync. Will respond after 50 milliseconds.
async fn setup_mocking_sliding_sync_server(server: &MockServer) -> MockGuard {
    let encryption_pos = Mutex::new(0);
    let room_pos = Mutex::new(0);

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
    let (client, server) = logged_in_client().await;

    let _guard = setup_mocking_sliding_sync_server(&server).await;

    let sync_service =
        SyncService::builder(client).with_encryption_sync(false, None).build().await.unwrap();

    // At first, the sync service is sleeping.
    assert_eq!(sync_service.state().get(), SyncServiceState::Idle);
    assert!(server.received_requests().await.unwrap().is_empty());

    // After starting, the sync service is, well, running.
    sync_service.start().await?;
    assert_eq!(sync_service.state().get(), SyncServiceState::Running);

    // Restarting while started doesn't change the current state.
    sync_service.start().await?;
    assert_eq!(sync_service.state().get(), SyncServiceState::Running);

    // Let the server respond a few times.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Pausing will stop both syncs, after a bit of delay.
    sync_service.pause()?;

    // Since this is racy: either it's already terminated, or we wait a bit and then
    // it'll be terminated.
    let current_state = sync_service.state().get();
    if !matches!(current_state, SyncServiceState::Terminated) {
        assert_eq!(current_state, SyncServiceState::Running);

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(sync_service.state().get(), SyncServiceState::Terminated);
    }

    let mut num_encryption_sync_requests: i32 = 0;
    let mut num_room_list_requests = 0;
    let mut latest_room_list_pos = None;
    for request in &server.received_requests().await.expect("Request recording has been disabled") {
        if !SlidingSyncMatcher.matches(request) {
            continue;
        }

        let mut json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

        if let Some(root) = json_value.as_object_mut() {
            if let Some(conn_id) = root.get("conn_id").and_then(|obj| obj.as_str()) {
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
    }

    assert!(num_encryption_sync_requests > 0);
    assert!(num_room_list_requests > 0);
    assert!((num_encryption_sync_requests - num_room_list_requests).abs() <= 1);
    assert!(latest_room_list_pos.is_some());

    // Now reset the server, wait for a bit that it doesn't receive extra requests.
    drop(_guard);
    server.reset().await;
    let _guard = setup_mocking_sliding_sync_server(&server).await;

    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(server.received_requests().await.unwrap().is_empty());

    // When restarting and waiting a bit, the server gets new requests, starting at
    // the same position than just before being stopped.
    sync_service.start().await?;
    assert_eq!(sync_service.state().get(), SyncServiceState::Running);

    tokio::time::sleep(Duration::from_millis(100)).await;

    num_encryption_sync_requests = 0;
    num_room_list_requests = 0;

    for request in &server.received_requests().await.expect("Request recording has been disabled") {
        if !SlidingSyncMatcher.matches(request) {
            continue;
        }

        let mut json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

        if let Some(root) = json_value.as_object_mut() {
            if let Some(conn_id) = root.get("conn_id").and_then(|obj| obj.as_str()) {
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
    }

    assert!(num_encryption_sync_requests > 0);
    assert!(num_room_list_requests > 0);
    assert!((num_encryption_sync_requests - num_room_list_requests).abs() <= 1);

    Ok(())
}

#[async_test]
async fn test_sync_service_no_encryption_state() -> anyhow::Result<()> {
    let (client, _server) = logged_in_client().await;

    let sync_service = SyncService::builder(client).build().await.unwrap();
    let states = Arc::new(Mutex::new(Vec::new()));

    let mut state = sync_service.state();

    let task_states = states.clone();
    let handle = tokio::spawn(async move {
        while let Some(state) = state.next().await {
            task_states.lock().unwrap().push(state);
        }
    });

    // At first, the sync service is sleeping.
    assert_eq!(sync_service.state().get(), SyncServiceState::Idle);

    // After starting, the sync service will be running for a very short while, then
    // getting into the error state.
    sync_service.start().await?;

    // Wait a bit for the underlying sync to fail (server isn't mocked here, so will
    // 404).
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Drop the service so the stream spawned in the task is aborted too.
    drop(sync_service);
    handle.await?;

    let states = states.lock().unwrap();
    assert_eq!(*states, &[SyncServiceState::Running, SyncServiceState::Error,]);

    Ok(())
}
