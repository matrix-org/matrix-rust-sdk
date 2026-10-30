use std::time::Duration;

use matrix_sdk::{
    BeaconError, deserialized_responses::SyncOrStrippedState, test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{
    DEFAULT_TEST_ROOM_ID, JoinedRoomBuilder, async_test, event_factory::EventFactory,
};
use ruma::{
    MilliSecondsSinceUnixEpoch, event_id,
    events::{StateEventType, beacon_info::BeaconInfoEventContent, location::AssetType},
    user_id,
};
use serde_json::json;
use strass::{assert_let, assert_variant_eq};

#[async_test]
async fn test_start_live_location_share_for_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let f = EventFactory::new();
    let event_id = event_id!("$h29iv0s8:example.com");
    let room_id = *DEFAULT_TEST_ROOM_ID;

    // Validate request body and response, partial body matching due to
    // auto-generated `org.matrix.msc3488.ts`.
    server
        .mock_room_send_state()
        .for_type(StateEventType::BeaconInfo)
        .body_matches_partial_json(json!({
            "description": "Live Share",
            "live": true,
            "timeout": 3000,
            "org.matrix.msc3488.asset": { "type": "m.self" }
        }))
        .ok(event_id)
        .mock_once()
        .mount()
        .await;

    let room = server.sync_joined_room(&client, room_id).await;

    let response =
        room.start_live_location_share(3000, Some("Live Share".to_owned())).await.unwrap();

    assert_eq!(response.event_id, event_id);

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(
                f.beacon_info(
                    Some("Live Share".to_owned()),
                    Duration::from_millis(3000),
                    true,
                    None,
                )
                .event_id(event_id)
                .sender(user_id)
                .state_key(user_id)
                .into_raw_sync_state(),
            ),
        )
        .await;

    let state_events = room.get_state_events_static::<BeaconInfoEventContent>().await.unwrap();
    assert_eq!(state_events.len(), 1);
    assert_let!(
        Ok(SyncOrStrippedState::Sync(ev)) = state_events[0].deserialize(),
        "Failed to deserialize event"
    );

    assert_eq!(ev.sender(), user_id);
    assert_eq!(ev.state_key(), user_id);
    assert_eq!(ev.event_id(), event_id);

    let content = &ev.as_original().unwrap().content;
    assert_variant_eq!(content.description, Some("Live Share"));
    assert_eq!(content.timeout, Duration::from_millis(3000));
    assert_eq!(content.asset.type_, AssetType::Self_);
    assert!(content.live);
}

#[async_test]
async fn test_stop_sharing_live_location() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let f = EventFactory::new();
    let event_id = event_id!("$h29iv0s8:example.com");
    let room_id = *DEFAULT_TEST_ROOM_ID;

    // Validate request body and response, partial body matching due to
    // auto-generated `org.matrix.msc3488.ts`.
    server
        .mock_room_send_state()
        .for_type(StateEventType::BeaconInfo)
        .body_matches_partial_json(json!({
            "description": "Live Share",
            "live": false,
            "timeout": 3000,
            "org.matrix.msc3488.asset": { "type": "m.self" }
        }))
        .ok(event_id)
        .mock_once()
        .mount()
        .await;

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(
                f.beacon_info(
                    Some("Live Share".to_owned()),
                    Duration::from_millis(3000),
                    true,
                    None,
                )
                .sender(user_id)
                .state_key(user_id)
                .into_raw_sync_state(),
            ),
        )
        .await;

    let response = room.stop_live_location_share().await.unwrap();
    assert_eq!(response.event_id, event_id);

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(
                f.beacon_info(
                    Some("Live Share".to_owned()),
                    Duration::from_millis(3000),
                    false,
                    None,
                )
                .event_id(event_id)
                .sender(user_id)
                .state_key(user_id)
                .into_raw_sync_state(),
            ),
        )
        .await;

    let state_events = room.get_state_events_static::<BeaconInfoEventContent>().await.unwrap();
    assert_eq!(state_events.len(), 1);
    assert_let!(
        Ok(SyncOrStrippedState::Sync(ev)) = state_events[0].deserialize(),
        "Failed to deserialize event"
    );

    assert_eq!(ev.sender(), user_id);
    assert_eq!(ev.state_key(), user_id);
    assert_eq!(ev.event_id(), event_id);

    let content = &ev.as_original().unwrap().content;
    assert_variant_eq!(content.description, Some("Live Share"));
    assert_eq!(content.timeout, Duration::from_millis(3000));
    assert_eq!(content.asset.type_, AssetType::Self_);
    assert!(!content.live);
}

#[async_test]
async fn test_stop_sharing_live_location_fails_if_not_live() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let current_time = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    let beacon_info_event = f
        .beacon_info(
            Some("Live Share".to_owned()),
            Duration::from_millis(3000),
            false,
            Some(current_time),
        )
        .event_id(event_id!("$15139375514XsgmR:localhost"))
        .sender(user_id!("@example:localhost"))
        .state_key(user_id!("@example:localhost"))
        .into_raw_sync_state();

    server
        .mock_room_send_state()
        .body_matches_partial_json(json!({
            "description": "Live Share",
            "live": false,
            "timeout": 3000,
            "org.matrix.msc3488.asset": { "type": "m.self" }
        }))
        .ok(event_id!("$h29iv0s8:example.com"))
        .expect(0)
        .mount()
        .await;

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_state_event(beacon_info_event),
        )
        .await;

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();

    assert!(matches!(room.stop_live_location_share().await, Err(BeaconError::NotLive)));
}
