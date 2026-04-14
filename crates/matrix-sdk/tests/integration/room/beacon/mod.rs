use std::time::Duration;

use futures_util::{FutureExt, StreamExt as _, pin_mut};
use js_int::uint;
use matrix_sdk::{
    assert_let_timeout, live_location_share::LiveLocationShare, test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{
    DEFAULT_TEST_ROOM_ID, JoinedRoomBuilder, async_test, event_factory::EventFactory,
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, event_id, events::location::AssetType, owned_event_id,
    user_id,
};
use serde_json::json;

#[async_test]
async fn test_send_location_beacon() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    // Validate request body and response, partial body matching due to
    // auto-generated `org.matrix.msc3488.ts`.
    server
        .mock_room_send()
        .body_matches_partial_json(json!({
            "m.relates_to": {
                "event_id": "$15139375514XsgmR:localhost",
                "rel_type": "m.reference"
            },
             "org.matrix.msc3488.location": {
                "uri": "geo:48.8588448,2.2943506"
            }
        }))
        .ok(event_id!("$h29iv0s8:example.com"))
        .mock_once()
        .mount()
        .await;

    let current_time = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    let beacon_info_event = f
        .beacon_info(
            Some("Live Share".to_owned()),
            Duration::from_millis(600_000),
            true,
            Some(current_time),
        )
        .event_id(event_id!("$15139375514XsgmR:localhost"))
        .sender(user_id!("@example:localhost"))
        .state_key(user_id!("@example:localhost"))
        .into_raw_sync_state();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_state_event(beacon_info_event),
        )
        .await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response = room.send_location_beacon("geo:48.8588448,2.2943506".to_owned()).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn test_send_location_beacon_fails_without_starting_live_share() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_joined_room(&client, *DEFAULT_TEST_ROOM_ID).await;
    let response = room.send_location_beacon("geo:48.8588448,2.2943506".to_owned()).await;

    assert!(response.is_err());
}

#[async_test]
async fn test_send_location_beacon_with_expired_live_share() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let f = EventFactory::new();

    let beacon_info_event = f
        .beacon_info(
            Some("Live Share".to_owned()),
            Duration::from_millis(3000),
            false,
            Some(MilliSecondsSinceUnixEpoch(uint!(1_636_829_458))),
        )
        .event_id(event_id!("$15139375514XsgmR:localhost"))
        .sender(user_id!("@example2:localhost"))
        .state_key(user_id!("@example2:localhost"))
        .into_raw_sync_state();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_state_event(beacon_info_event),
        )
        .await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response = room.send_location_beacon("geo:48.8588448,2.2943506".to_owned()).await;

    assert!(response.is_err());
}

#[async_test]
async fn test_most_recent_event_in_stream() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let current_time = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    let beacon_info_event = f
        .beacon_info(
            Some("Live Share".to_owned()),
            Duration::from_millis(3_600_000),
            true,
            Some(current_time),
        )
        .event_id(event_id!("$15139375514XsgmR:localhost"))
        .sender(user_id!("@example2:localhost"))
        .state_key(user_id!("@example2:localhost"))
        .into_raw_sync_state();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_state_event(beacon_info_event),
        )
        .await;

    // Enable the event cache so the initial snapshot can be loaded from it.
    client.event_cache().subscribe().unwrap();

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();

    let mut timeline_events = Vec::new();

    for nth in 0..10 {
        let event_id = format!("$event_for_stream_{nth}");
        timeline_events.push(
            f.beacon(
                owned_event_id!("$15139375514XsgmR:localhost"),
                nth as f64 + 0.9575274619722,
                12.494122581370175,
                nth,
                Some(MilliSecondsSinceUnixEpoch(1_636_829_458u32.into())),
            )
            .event_id(<&EventId>::try_from(event_id.as_str()).unwrap())
            .server_ts(1_636_829_458)
            .sender(user_id!("@example2:localhost"))
            .age(598971)
            .into_raw_sync(),
        );
    }

    let mut event_cache_updates_stream = client.event_cache().subscribe_to_room_generic_updates();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_bulk(timeline_events),
        )
        .await;

    // Wait for the event cache background task to commit the events before
    // querying.
    assert_let_timeout!(Ok(_) = event_cache_updates_stream.recv());

    // Create the stream after syncing all beacon events — the initial snapshot is
    // loaded from the event cache and already reflects the latest beacon.
    let live_location_shares = room.live_location_shares().await;
    let (mut shares, _stream) = live_location_shares.subscribe();

    assert_eq!(shares.len(), 1);
    let LiveLocationShare { user_id, last_location, beacon_info, .. } = shares.remove(0);

    assert_eq!(user_id.to_string(), "@example2:localhost");

    let last_location = last_location.expect("Expected last location");
    assert_eq!(
        last_location.location.uri,
        format!("geo:{},{};u=9", 9.9575274619722, 12.494122581370175)
    );

    assert!(last_location.location.description.is_none());
    assert!(last_location.location.zoom_level.is_none());
    assert_eq!(last_location.ts, MilliSecondsSinceUnixEpoch(uint!(1_636_829_458)));

    assert!(beacon_info.live);
    assert!(beacon_info.is_live());
    assert_eq!(beacon_info.description, Some("Live Share".to_owned()));
    assert_eq!(beacon_info.timeout, Duration::from_millis(3_600_000));
    assert_eq!(beacon_info.ts, current_time);
    assert_eq!(beacon_info.asset.type_, AssetType::Self_);
}

#[async_test]
async fn test_observe_single_live_location_share() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let current_time = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    let beacon_info_event = f
        .beacon_info(
            Some("Test Live Share".to_owned()),
            Duration::from_millis(3_600_000),
            true,
            Some(current_time),
        )
        .event_id(event_id!("$test_beacon_info"))
        .sender(user_id!("@example2:localhost"))
        .state_key(user_id!("@example2:localhost"))
        .into_raw_sync_state();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_state_event(beacon_info_event),
        )
        .await;

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();
    let live_location_shares = room.live_location_shares().await;
    let (initial, stream) = live_location_shares.subscribe();
    pin_mut!(stream);

    // Initial snapshot contains the beacon_info from state (no last_location yet).
    assert_eq!(initial.len(), 1);
    assert!(initial[0].last_location.is_none());

    let timeline_event = f
        .beacon(owned_event_id!("$test_beacon_info"), 10.0, 20.0, 5, Some(current_time))
        .event_id(event_id!("$location_event"))
        .server_ts(current_time)
        .sender(user_id!("@example2:localhost"))
        .into_raw_sync();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_event(timeline_event),
        )
        .await;

    let diffs = stream.next().await.expect("Expected live location share update");
    let mut shares = diffs.into_iter().fold(initial, |mut v, diff| {
        diff.apply(&mut v);
        v
    });
    // Beacon handler updates the existing share with last_location
    assert_eq!(shares.len(), 1);
    let LiveLocationShare { user_id, last_location, beacon_info, .. } = shares.remove(0);

    assert_eq!(user_id.to_string(), "@example2:localhost");
    let last_location = last_location.expect("Expected last location");
    assert_eq!(last_location.location.uri, "geo:10,20;u=5");
    assert_eq!(last_location.ts, current_time);

    assert!(beacon_info.live);
    assert!(beacon_info.is_live());
    assert_eq!(beacon_info.description, Some("Test Live Share".to_owned()));
    assert_eq!(beacon_info.timeout, Duration::from_millis(3_600_000));
    assert_eq!(beacon_info.ts, current_time);
}

#[async_test]
async fn test_observing_live_location_does_not_return_non_live() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let f = EventFactory::new();

    // The user's beacon_info is NOT live.
    let beacon_info_event = f
        .beacon_info(None, Duration::from_millis(60_000), false, None)
        .event_id(event_id!("$15139375514XsgmR:localhost"))
        .sender(user_id!("@example:localhost"))
        .state_key(user_id!("@example:localhost"))
        .into_raw_sync_state();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_state_event(beacon_info_event),
        )
        .await;

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();
    let live_location_shares = room.live_location_shares().await;
    let (initial, stream) = live_location_shares.subscribe();
    pin_mut!(stream);

    // Initial is empty because beacon_info is not live.
    assert!(initial.is_empty());

    // A beacon event arrives, but beacon_info is not live — should not yield.
    let beacon_event = f
        .beacon(owned_event_id!("$15139375514XsgmR:localhost"), 51.5008, 0.1247, 35, None)
        .event_id(event_id!("$152037dfsef280074GZeOm:localhost"))
        .sender(user_id!("@example:localhost"))
        .into_raw_sync();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_event(beacon_event),
        )
        .await;

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_location_update_for_already_tracked_user() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let now = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    // Set up alice with a live beacon_info.
    let beacon_info_event = f
        .beacon_info(
            Some("Alice location".to_owned()),
            Duration::from_millis(300_000),
            true,
            Some(now),
        )
        .event_id(event_id!("$alice_beacon_info"))
        .sender(user_id!("@alice:localhost"))
        .state_key(user_id!("@alice:localhost"))
        .into_raw_sync_state();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_state_event(beacon_info_event),
        )
        .await;

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();
    let live_location_shares = room.live_location_shares().await;
    let (initial, stream) = live_location_shares.subscribe();
    pin_mut!(stream);

    // Initial snapshot contains the beacon_info from state (no last_location yet).
    assert_eq!(initial.len(), 1);
    assert!(initial[0].last_location.is_none());

    // Alice's first beacon — updates the existing share with last_location.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_event(
                f.beacon(owned_event_id!("$alice_beacon_info"), 10.0, 20.0, 5, None)
                    .event_id(event_id!("$loc1"))
                    .sender(user_id!("@alice:localhost"))
                    .server_ts(now)
                    .into_raw_sync(),
            ),
        )
        .await;

    let diffs = stream.next().await.expect("Expected first location");
    let shares = diffs.into_iter().fold(initial, |mut v, diff| {
        diff.apply(&mut v);
        v
    });
    assert_eq!(shares.len(), 1);
    let share = &shares[0];
    assert_eq!(share.user_id, user_id!("@alice:localhost"));
    let last_location = share.last_location.as_ref().expect("Expected last location");
    assert_eq!(last_location.location.uri, "geo:10,20;u=5");
    assert_eq!(share.beacon_info.description, Some("Alice location".to_owned()));

    // Alice's second beacon — already tracked, beacon_info is reused from cache.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_event(
                f.beacon(owned_event_id!("$alice_beacon_info"), 30.0, 40.0, 10, None)
                    .event_id(event_id!("$loc2"))
                    .sender(user_id!("@alice:localhost"))
                    .server_ts(now)
                    .into_raw_sync(),
            ),
        )
        .await;

    let diffs = stream.next().await.expect("Expected second location update");
    let shares = diffs.into_iter().fold(shares, |mut v, diff| {
        diff.apply(&mut v);
        v
    });
    assert_eq!(shares.len(), 1);
    let LiveLocationShare { user_id, last_location, beacon_info, .. } = shares[0].clone();
    assert_eq!(user_id, user_id!("@alice:localhost"));
    let last_location = last_location.expect("Expected last location");
    assert_eq!(last_location.location.uri, "geo:30,40;u=10");
    // beacon_info is preserved from the initial share — not re-fetched from state.
    assert_eq!(beacon_info.description, Some("Alice location".to_owned()));
}

#[async_test]
async fn test_beacon_info_stop_removes_user_from_stream() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let now = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    // Alice starts with a live beacon_info.
    let beacon_info_event = f
        .beacon_info(None, Duration::from_millis(300_000), true, Some(now))
        .event_id(event_id!("$alice_beacon_info"))
        .sender(user_id!("@alice:localhost"))
        .state_key(user_id!("@alice:localhost"))
        .into_raw_sync_state();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_state_event(beacon_info_event),
        )
        .await;

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();
    let live_location_shares = room.live_location_shares().await;
    let (initial, stream) = live_location_shares.subscribe();
    pin_mut!(stream);

    // Initial snapshot contains the beacon_info from state (no last_location yet).
    assert_eq!(initial.len(), 1);
    assert!(initial[0].last_location.is_none());

    // Alice stops her share — beacon_info timeline event with live: false.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_event(
                f.beacon_info(None, Duration::from_secs(300), false, None)
                    .event_id(event_id!("$alice_beacon_info_stop"))
                    .sender(user_id!("@alice:localhost"))
                    .state_key(user_id!("@alice:localhost"))
                    .into_raw_sync(),
            ),
        )
        .await;

    // Stream emits an update with an empty list — alice is removed.
    let diffs = stream.next().await.expect("Expected share removal");
    let shares = diffs.into_iter().fold(initial, |mut v, diff| {
        diff.apply(&mut v);
        v
    });
    assert!(shares.is_empty());
}

#[async_test]
async fn test_multiple_users_in_stream() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let now = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    // Both alice and bob have live beacon_infos.
    let alice_beacon_info = f
        .beacon_info(None, Duration::from_millis(300_000), true, Some(now))
        .event_id(event_id!("$alice_beacon_info"))
        .sender(user_id!("@alice:localhost"))
        .state_key(user_id!("@alice:localhost"))
        .into_raw_sync_state();

    let bob_beacon_info = f
        .beacon_info(None, Duration::from_millis(300_000), true, Some(now))
        .event_id(event_id!("$bob_beacon_info"))
        .sender(user_id!("@bob:localhost"))
        .state_key(user_id!("@bob:localhost"))
        .into_raw_sync_state();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID)
                .add_state_event(alice_beacon_info)
                .add_state_event(bob_beacon_info),
        )
        .await;

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();
    let live_location_shares = room.live_location_shares().await;
    let (initial, stream) = live_location_shares.subscribe();
    pin_mut!(stream);

    // Initial snapshot contains both alice and bob beacon_infos from state.
    assert_eq!(initial.len(), 2);

    // Alice's beacon arrives — updates her existing share with last_location.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_event(
                f.beacon(owned_event_id!("$alice_beacon_info"), 10.0, 20.0, 5, None)
                    .event_id(event_id!("$alice_loc"))
                    .sender(user_id!("@alice:localhost"))
                    .server_ts(now)
                    .into_raw_sync(),
            ),
        )
        .await;

    let diffs = stream.next().await.expect("Expected alice's location");
    let shares = diffs.into_iter().fold(initial, |mut v, diff| {
        diff.apply(&mut v);
        v
    });
    assert_eq!(shares.len(), 2);

    // Bob's beacon arrives — updates his existing share with last_location.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_event(
                f.beacon(owned_event_id!("$bob_beacon_info"), 50.0, 60.0, 8, None)
                    .event_id(event_id!("$bob_loc"))
                    .sender(user_id!("@bob:localhost"))
                    .server_ts(now)
                    .into_raw_sync(),
            ),
        )
        .await;

    let diffs = stream.next().await.expect("Expected both users");
    let shares = diffs.into_iter().fold(shares, |mut v, diff| {
        diff.apply(&mut v);
        v
    });
    assert_eq!(shares.len(), 2);
    let mut shares: Vec<_> = shares.into_iter().collect();
    shares.sort_by_key(|s| s.user_id.clone());

    assert_eq!(shares[0].user_id, user_id!("@alice:localhost"));
    assert_eq!(
        shares[0].last_location.as_ref().expect("Expected last location").location.uri,
        "geo:10,20;u=5"
    );

    assert_eq!(shares[1].user_id, user_id!("@bob:localhost"));
    assert_eq!(
        shares[1].last_location.as_ref().expect("Expected last location").location.uri,
        "geo:50,60;u=8"
    );
}

#[async_test]
async fn test_initial_load_contains_location_from_event_cache() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Enable event cache BEFORE syncing so events get cached.
    client.event_cache().subscribe().unwrap();

    let now = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    // Alice has a live beacon_info.
    let beacon_info_event = f
        .beacon_info(
            Some("Alice location".to_owned()),
            Duration::from_millis(300_000),
            true,
            Some(now),
        )
        .event_id(event_id!("$alice_beacon_info"))
        .sender(user_id!("@alice:localhost"))
        .state_key(user_id!("@alice:localhost"))
        .into_raw_sync_state();

    // A beacon event in the timeline.
    let beacon_event = f
        .beacon(owned_event_id!("$alice_beacon_info"), 48.8566, 2.3522, 10, Some(now))
        .event_id(event_id!("$alice_location"))
        .sender(user_id!("@alice:localhost"))
        .server_ts(now)
        .into_raw_sync();

    let mut event_cache_updates_stream = client.event_cache().subscribe_to_room_generic_updates();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID)
                .add_state_event(beacon_info_event)
                .add_timeline_event(beacon_event),
        )
        .await;

    // Wait for the event cache background task to commit the events before
    // querying.
    assert_let_timeout!(Ok(_) = event_cache_updates_stream.recv());

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();
    let live_location_shares = room.live_location_shares().await;
    let (initial, _stream) = live_location_shares.subscribe();

    // Initial snapshot should contain both beacon_info AND last_location.
    assert_eq!(initial.len(), 1);
    let share = &initial[0];
    assert_eq!(share.user_id, user_id!("@alice:localhost"));
    assert_eq!(share.beacon_info.description, Some("Alice location".to_owned()));

    let last_location =
        share.last_location.as_ref().expect("Expected last_location in initial load");
    assert_eq!(last_location.location.uri, "geo:48.8566,2.3522;u=10");
    assert_eq!(last_location.ts, now);
}
