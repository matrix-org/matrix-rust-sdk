use std::time::{Duration, UNIX_EPOCH};

use js_int::uint;
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{
    async_test, mocks::mock_encryption_state, sync_timeline_event, test_json, JoinedRoomBuilder,
    SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
};
use ruma::{event_id, time::SystemTime, MilliSecondsSinceUnixEpoch};
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
async fn test_subscribe_to_live_location_shares() {
    let (client, server) = logged_in_client_with_server().await;

    let mut sync_builder = SyncResponseBuilder::new();

    // Get the current timestamp for the `beacon_info` event.
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
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(*DEFAULT_TEST_ROOM_ID).unwrap();

    let (task_handle, mut subscriber) = room.subscribe_to_live_location_shares();

    sync_builder.add_joined_room(JoinedRoomBuilder::new(*DEFAULT_TEST_ROOM_ID).add_timeline_event(
        sync_timeline_event!({
           "content": {
                "m.relates_to": {
                  "event_id": "$TlS7h0NHzBdZIccsSspF5CMpQE8YMT0stRern0nXscI",
                  "rel_type": "m.reference"
                },
                "org.matrix.msc3488.location": {
                  "uri": "geo:8.95752746197222,12.494122581370175;u=10"
                },
                "org.matrix.msc3488.ts": 1_636_829_458
            },
            "event_id": "$152037280074GZeOm:localhost",
            "origin_server_ts": 1_636_829_458,
            "sender": "@example:localhost",
            "type": "org.matrix.msc3672.beacon",
            "unsigned": {
                "age": 598971
            }
        }),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let live_location_share =
        subscriber.recv().await.expect("Failed to receive live location share");

    assert_eq!(live_location_share.user_id.to_string(), "@example:localhost");

    assert_eq!(
        live_location_share.last_location.location.uri,
        "geo:8.95752746197222,12.494122581370175;u=10"
    );
    assert!(live_location_share.last_location.location.description.is_none());
    assert!(live_location_share.last_location.location.zoom_level.is_none());
    assert_eq!(
        live_location_share.last_location.ts,
        MilliSecondsSinceUnixEpoch(uint!(1_636_829_458))
    );

    task_handle.await.unwrap();
}
