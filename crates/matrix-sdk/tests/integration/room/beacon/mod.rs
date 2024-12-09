use std::time::{Duration, UNIX_EPOCH};

use futures_util::{pin_mut, StreamExt as _};
use js_int::uint;
use matrix_sdk::{config::SyncSettings, live_location_share::LiveLocationShare};
use matrix_sdk_test::{
    async_test, mocks::mock_encryption_state, sync_timeline_event, test_json, JoinedRoomBuilder,
    SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
};
use ruma::{event_id, events::location::AssetType, time::SystemTime, MilliSecondsSinceUnixEpoch};
use serde_json::json;
use wiremock::{
    matchers::{body_partial_json, header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client_with_server, mock_sync};
#[async_test]
async fn test_send_location_beacon() {
    let (client, server) = logged_in_client_with_server().await;

    // Validate request body and response, partial body matching due to
    // auto-generated `org.matrix.msc3488.ts`.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/org.matrix.msc3672.beacon/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "m.relates_to": {
                "event_id": "$15139375514XsgmR:localhost",
                "rel_type": "m.reference"
            },
             "org.matrix.msc3488.location": {
                "uri": "geo:48.8588448,2.2943506"
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    let current_timestamp =
        SystemTime::now().duration_since(UNIX_EPOCH).expect("Time went backwards").as_millis()
            as u64;

    mock_sync(
        &server,
        json!({
            "next_batch": "s526_47314_0_7_1_1_1_1_1",
            "rooms": {
                "join": {
                    *DEFAULT_TEST_ROOM_ID: {
                        "state": {
                            "events": [
                                {
                                    "content": {
                                        "description": "Live Share",
                                        "live": true,
                                        "org.matrix.msc3488.ts": current_timestamp,
                                        "timeout": 600_000,
                                        "org.matrix.msc3488.asset": { "type": "m.self" }
                                    },
                                    "event_id": "$15139375514XsgmR:localhost",
                                    "origin_server_ts": 1_636_829_458,
                                    "sender": "@example:localhost",
                                    "state_key": "@example:localhost",
                                    "type": "org.matrix.msc3672.beacon_info",
                                    "unsigned": {
                                        "age": 7034220
                                    }
                                },
                            ]
                        }
                    }
                }
            }

        }),
        None,
    )
    .await;

    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response = room.send_location_beacon("geo:48.8588448,2.2943506".to_owned()).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn test_send_location_beacon_fails_without_starting_live_share() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response = room.send_location_beacon("geo:48.8588448,2.2943506".to_owned()).await;

    assert!(response.is_err());
}

#[async_test]
async fn test_send_location_beacon_with_expired_live_share() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(
        &server,
        json!({
            "next_batch": "s526_47314_0_7_1_1_1_1_1",
            "rooms": {
                "join": {
                    *DEFAULT_TEST_ROOM_ID: {
                        "state": {
                            "events": [
                                {
                                    "content": {
                                        "description": "Live Share",
                                        "live": false,
                                        "org.matrix.msc3488.ts": 1_636_829_458,
                                        "timeout": 3000,
                                        "org.matrix.msc3488.asset": { "type": "m.self" }
                                    },
                                    "event_id": "$15139375514XsgmR:localhost",
                                    "origin_server_ts": 1_636_829_458,
                                    "sender": "@example:localhost",
                                    "state_key": "@example:localhost",
                                    "type": "org.matrix.msc3672.beacon_info",
                                    "unsigned": {
                                        "age": 7034220
                                    }
                                },
                            ]
                        }
                    }
                }
            }

        }),
        None,
    )
    .await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response = room.send_location_beacon("geo:48.8588448,2.2943506".to_owned()).await;

    assert!(response.is_err());
}

#[async_test]
async fn test_most_recent_event_in_stream() {
    let (client, server) = logged_in_client_with_server().await;

    let mut sync_builder = SyncResponseBuilder::new();

    let current_time = MilliSecondsSinceUnixEpoch::now();
    let millis_time = current_time
        .to_system_time()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64;

    mock_sync(
        &server,
        json!({
            "next_batch": "s526_47314_0_7_1_1_1_1_1",
            "rooms": {
                "join": {
                    *DEFAULT_TEST_ROOM_ID: {
                        "state": {
                            "events": [
                                {
                                    "content": {
                                        "description": "Live Share",
                                        "live": true,
                                        "org.matrix.msc3488.ts": millis_time,
                                        "timeout": 3000,
                                        "org.matrix.msc3488.asset": { "type": "m.self" }
                                    },
                                    "event_id": "$15139375514XsgmR:localhost",
                                    "origin_server_ts": millis_time,
                                    "sender": "@example:localhost",
                                    "state_key": "@example:localhost",
                                    "type": "org.matrix.msc3672.beacon_info",
                                    "unsigned": {
                                        "age": 7034220
                                    }
                                },
                            ]
                        }
                    }
                }
            }

        }),
        None,
    )
    .await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();

    let observable_live_location_shares = room.observe_live_location_shares();
    let stream = observable_live_location_shares.subscribe();
    pin_mut!(stream);

    let mut timeline_events = Vec::new();

    for nth in 0..25 {
        timeline_events.push(sync_timeline_event!({
            "content": {
                "m.relates_to": {
                    "event_id": "$15139375514XsgmR:localhost",
                    "rel_type": "m.reference"
                },
                "org.matrix.msc3488.location": {
                    "uri": format!("geo:{nth}.9575274619722,12.494122581370175;u={nth}")
                },
                "org.matrix.msc3488.ts": 1_636_829_458
            },
            "event_id": format!("$event_for_stream_{nth}"),
            "origin_server_ts": 1_636_829_458,
            "sender": "@example:localhost",
            "type": "org.matrix.msc3672.beacon",
            "unsigned": {
                "age": 598971
            }
        }));
    }

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_bulk(timeline_events),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Stream should only process the latest beacon event for the user, ignoring any
    // previous events.
    let LiveLocationShare { user_id, last_location, beacon_info } =
        stream.next().await.expect("Another live location was expected");

    assert_eq!(user_id.to_string(), "@example:localhost");

    assert_eq!(last_location.location.uri, "geo:24.9575274619722,12.494122581370175;u=24");

    assert!(last_location.location.description.is_none());
    assert!(last_location.location.zoom_level.is_none());
    assert_eq!(last_location.ts, MilliSecondsSinceUnixEpoch(uint!(1_636_829_458)));

    let beacon_info = beacon_info.expect("Live location share is missing the beacon_info");

    assert!(beacon_info.live);
    assert!(beacon_info.is_live());
    assert_eq!(beacon_info.description, Some("Live Share".to_owned()));
    assert_eq!(beacon_info.timeout, Duration::from_millis(3000));
    assert_eq!(beacon_info.ts, current_time);
    assert_eq!(beacon_info.asset.type_, AssetType::Self_);
}

#[async_test]
async fn test_observe_single_live_location_share() {
    let (client, server) = logged_in_client_with_server().await;

    let current_time = MilliSecondsSinceUnixEpoch::now();
    let millis_time = current_time
        .to_system_time()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_millis() as u64;

    mock_sync(
        &server,
        json!({
            "next_batch": "s526_47314_0_7_1_1_1_1_1",
            "rooms": {
                "join": {
                    *DEFAULT_TEST_ROOM_ID: {
                        "state": {
                            "events": [
                                {
                                    "content": {
                                        "description": "Test Live Share",
                                        "live": true,
                                        "org.matrix.msc3488.ts": millis_time,
                                        "timeout": 3000,
                                        "org.matrix.msc3488.asset": { "type": "m.self" }
                                    },
                                    "event_id": "$test_beacon_info",
                                    "origin_server_ts": millis_time,
                                    "sender": "@example:localhost",
                                    "state_key": "@example:localhost",
                                    "type": "org.matrix.msc3672.beacon_info",
                                }
                            ]
                        }
                    }
                }
            }
        }),
        None,
    )
    .await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();
    let observable_live_location_shares = room.observe_live_location_shares();
    let stream = observable_live_location_shares.subscribe();
    pin_mut!(stream);

    let timeline_event = sync_timeline_event!({
        "content": {
            "m.relates_to": {
                "event_id": "$test_beacon_info",
                "rel_type": "m.reference"
            },
            "org.matrix.msc3488.location": {
                "uri": "geo:10.000000,20.000000;u=5"
            },
            "org.matrix.msc3488.ts": 1_636_829_458
        },
        "event_id": "$location_event",
        "origin_server_ts": millis_time,
        "sender": "@example:localhost",
        "type": "org.matrix.msc3672.beacon",
    });

    mock_sync(
        &server,
        SyncResponseBuilder::new()
            .add_joined_room(
                JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_event(timeline_event),
            )
            .build_json_sync_response(),
        None,
    )
    .await;

    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let LiveLocationShare { user_id, last_location, beacon_info } =
        stream.next().await.expect("Another live location was expected");

    assert_eq!(user_id.to_string(), "@example:localhost");
    assert_eq!(last_location.location.uri, "geo:10.000000,20.000000;u=5");
    assert_eq!(last_location.ts, current_time);

    let beacon_info = beacon_info.expect("Live location share is missing the beacon_info");

    assert!(beacon_info.live);
    assert!(beacon_info.is_live());
    assert_eq!(beacon_info.description, Some("Test Live Share".to_owned()));
    assert_eq!(beacon_info.timeout, Duration::from_millis(3000));
    assert_eq!(beacon_info.ts, current_time);
}
