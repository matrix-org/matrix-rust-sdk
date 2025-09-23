use std::time::Duration;

use js_int::uint;
use matrix_sdk::config::{SyncSettings, SyncToken};
use matrix_sdk_test::{DEFAULT_TEST_ROOM_ID, async_test, test_json};
use ruma::{
    MilliSecondsSinceUnixEpoch, event_id,
    events::{AnySyncStateEvent, StateEventType, location::AssetType},
};
use serde_json::json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_partial_json, header, method, path_regex},
};

use crate::{logged_in_client_with_server, mock_sync};

#[async_test]
async fn test_start_live_location_share_for_room() {
    let (client, server) = logged_in_client_with_server().await;

    // Validate request body and response, partial body matching due to
    // auto-generated `org.matrix.msc3488.ts`.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/org.matrix.msc3672.beacon_info/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "description": "Live Share",
            "live": true,
            "timeout": 3000,
            "org.matrix.msc3488.asset": { "type": "m.self" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);

    mock_sync(&server, &*test_json::SYNC, None).await;

    client.sync_once(sync_settings.clone()).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response =
        room.start_live_location_share(3000, Some("Live Share".to_owned())).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id);
    server.reset().await;

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

    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let state_events = room.get_state_events(StateEventType::BeaconInfo).await.unwrap();
    assert_eq!(state_events.len(), 1);

    let raw_event = state_events.first().expect("There should be a beacon_info state event");

    let ev = raw_event.deserialize().expect("Failed to deserialize event");
    let Some(AnySyncStateEvent::BeaconInfo(ev)) = ev.as_sync() else {
        panic!("Expected a BeaconInfo event");
    };

    let content = ev.as_original().unwrap().content.clone();

    assert_eq!(ev.sender(), room.own_user_id());
    assert_eq!(ev.state_key(), "@example:localhost");
    assert_eq!(ev.event_id(), event_id!("$15139375514XsgmR:localhost"));
    assert_eq!(ev.event_type(), StateEventType::BeaconInfo);
    assert_eq!(ev.origin_server_ts(), MilliSecondsSinceUnixEpoch(uint!(1_636_829_458)));

    assert_eq!(content.description, Some("Live Share".to_owned()));
    assert_eq!(content.timeout, Duration::from_millis(3000));
    assert_eq!(content.ts, MilliSecondsSinceUnixEpoch(uint!(1_636_829_458)));
    assert_eq!(content.asset.type_, AssetType::Self_);

    assert!(content.live);
}

#[async_test]
async fn test_stop_sharing_live_location() {
    let (client, server) = logged_in_client_with_server().await;

    // Validate request body and response, partial body matching due to
    // auto-generated `org.matrix.msc3488.ts`.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/org.matrix.msc3672.beacon_info/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "description": "Live Share",
            "live": false,
            "timeout": 3000,
            "org.matrix.msc3488.asset": { "type": "m.self" }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);

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

    client.sync_once(sync_settings.clone()).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response = room.stop_live_location_share().await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id);
    server.reset().await;

    mock_sync(
        &server,
        json!({
            "next_batch": "s526_47314_1_7_1_1_1_1_1",
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

    client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let state_events = room.get_state_events(StateEventType::BeaconInfo).await.unwrap();
    assert_eq!(state_events.len(), 1);

    let raw_event = state_events.first().expect("There should be a beacon_info state event");

    let ev = raw_event.deserialize().expect("Failed to deserialize event");
    let Some(AnySyncStateEvent::BeaconInfo(ev)) = ev.as_sync() else {
        panic!("Expected a BeaconInfo event");
    };

    let content = ev.as_original().unwrap().content.clone();

    assert_eq!(ev.sender(), room.own_user_id());
    assert_eq!(ev.state_key(), "@example:localhost");
    assert_eq!(ev.event_id(), event_id!("$15139375514XsgmR:localhost"));
    assert_eq!(ev.event_type(), StateEventType::BeaconInfo);
    assert_eq!(ev.origin_server_ts(), MilliSecondsSinceUnixEpoch(uint!(1_636_829_458)));

    assert_eq!(content.description, Some("Live Share".to_owned()));
    assert_eq!(content.timeout, Duration::from_millis(3000));
    assert_eq!(content.ts, MilliSecondsSinceUnixEpoch(uint!(1_636_829_458)));
    assert_eq!(content.asset.type_, AssetType::Self_);

    assert!(!content.live);
}
