use std::time::Duration;

use matrix_sdk::{config::SyncSettings, DisplayName, RoomMember};
use matrix_sdk_test::{async_test, test_json};
use ruma::{
    event_id,
    events::{AnySyncStateEvent, StateEventType},
    room_id,
};
use serde_json::{json, Value as JsonValue};
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync};

#[async_test]
async fn user_presence() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/members"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::MEMBERS))
        .mount(&server)
        .await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();
    let members: Vec<RoomMember> = room.active_members().await.unwrap();

    assert_eq!(2, members.len());
    // assert!(room.power_levels.is_some())
}

#[async_test]
async fn calculate_room_names_from_summary() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::DEFAULT_SYNC_SUMMARY, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();
    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    assert_eq!(DisplayName::Calculated("example2".to_owned()), room.display_name().await.unwrap());
}

#[async_test]
async fn room_names() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    assert_eq!(client.rooms().len(), 1);
    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    assert_eq!(DisplayName::Aliased("tutorial".to_owned()), room.display_name().await.unwrap());

    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, &*test_json::INVITE_SYNC, Some(sync_token.clone())).await;

    let _response = client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(client.rooms().len(), 1);
    let invited_room = client.get_invited_room(room_id!("!696r7674:example.com")).unwrap();

    assert_eq!(
        DisplayName::Named("My Room Name".to_owned()),
        invited_room.display_name().await.unwrap()
    );
}

#[async_test]
async fn test_state_event_getting() {
    let room_id = &test_json::DEFAULT_SYNC_ROOM_ID;

    let (client, server) = logged_in_client().await;

    let sync = json!({
        "next_batch": "1234",
        "rooms": {
            "join": {
                "!SVkFJHzfwvuaIEawgC:localhost": {
                    "state": {
                      "events": [
                        {
                          "type": "m.custom.note",
                          "sender": "@example:localhost",
                          "content": {
                            "body": "Note 1",
                          },
                          "state_key": "note.1",
                          "origin_server_ts": 1611853078727u64,
                          "unsigned": {
                            "replaces_state": "$2s9GcbVxbbFS3EZY9vN1zhavaDJnF32cAIGAxi99NuQ",
                            "age": 15458166523u64
                          },
                          "event_id": "$NVCTvrlxodf3ZGjJ6foxepEq8ysSkTq8wG0wKeQBVZg"
                        },
                        {
                          "type": "m.custom.note",
                          "sender": "@example2:localhost",
                          "content": {
                            "body": "Note 2",
                          },
                          "state_key": "note.2",
                          "origin_server_ts": 1611853078727u64,
                          "unsigned": {
                            "replaces_state": "$2s9GcbVxbbFS3EZY9vN1zhavaDJnF32cAIGAxi99NuQ",
                            "age": 15458166523u64
                          },
                          "event_id": "$NVCTvrlxodf3ZGjJ6foxepEq8ysSkTq8wG0wKeQBVZg"
                        },
                        {
                          "type": "m.room.encryption",
                          "sender": "@example:localhost",
                          "content": {
                            "algorithm": "m.megolm.v1.aes-sha2"
                          },
                          "state_key": "",
                          "origin_server_ts": 1586437448151u64,
                          "unsigned": {
                            "age": 40873797099u64
                          },
                          "event_id": "$vyG3wu1QdJSh5gc-09SwjXBXlXo8gS7s4QV_Yxha0Xw"
                        },
                      ]
                    }
                }
            }
        }
    });

    mock_sync(&server, sync, None).await;

    let room = client.get_joined_room(room_id);
    assert!(room.is_none());

    client.sync_once(SyncSettings::default()).await.unwrap();

    let room = client.get_joined_room(room_id).unwrap();

    let state_events = room.get_state_events(StateEventType::RoomEncryption).await.unwrap();
    assert_eq!(state_events.len(), 1);

    let state_events = room.get_state_events("m.custom.note".into()).await.unwrap();
    assert_eq!(state_events.len(), 2);

    let encryption_event = room
        .get_state_event(StateEventType::RoomEncryption, "")
        .await
        .unwrap()
        .unwrap()
        .deserialize()
        .unwrap();

    matches::assert_matches!(encryption_event, AnySyncStateEvent::RoomEncryption(_));
}

// FIXME: removing timelines during reading the stream currently leaves to an
// inconsistent undefined state. This tests shows that, but because
// different implementations deal with problem in different,
// inconsistent manners, isn't activated.
//#[async_test]
#[allow(dead_code)]
#[cfg(feature = "experimental-timeline")]
async fn room_timeline_with_remove() {
    use futures_util::StreamExt;
    use matrix_sdk::deserialized_responses::SyncRoomEvent;
    use wiremock::matchers::query_param;

    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    mock_sync(&server, &*test_json::SYNC, None).await;

    let _ = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();
    let (forward_stream, backward_stream) = room.timeline().await.unwrap();

    // these two syncs lead to the store removing its existing timeline
    // and replace them with new ones
    mock_sync(&server, &*test_json::MORE_SYNC, Some("s526_47314_0_7_1_1_1_11444_1".to_owned()))
        .await;
    mock_sync(&server, &*test_json::MORE_SYNC_2, Some("s526_47314_0_7_1_1_1_11444_2".to_owned()))
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "t392-516_47314_0_7_1_1_1_11444_1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::SYNC_ROOM_MESSAGES_BATCH_1),
        )
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "t47409-4357353_219380_26003_2269"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::SYNC_ROOM_MESSAGES_BATCH_2),
        )
        .expect(1)
        .named("messages_batch_2")
        .mount(&server)
        .await;

    assert_eq!(client.sync_token().await, Some("s526_47314_0_7_1_1_1_11444_1".to_owned()));
    let sync_settings = SyncSettings::new()
        .timeout(Duration::from_millis(3000))
        .token("s526_47314_0_7_1_1_1_11444_1");
    let _ = client.sync_once(sync_settings).await.unwrap();

    let sync_settings = SyncSettings::new()
        .timeout(Duration::from_millis(3000))
        .token("s526_47314_0_7_1_1_1_11444_2");
    let _ = client.sync_once(sync_settings).await.unwrap();

    let expected_forward_events = vec![
        "$152037280074GZeOm:localhost",
        "$editevid:localhost",
        "$151957878228ssqrJ:localhost",
        "$15275046980maRLj:localhost",
        "$15275047031IXQRi:localhost",
        "$098237280074GZeOm:localhost",
        "$152037280074GZeOm2:localhost",
        "$editevid2:localhost",
        "$151957878228ssqrJ2:localhost",
        "$15275046980maRLj2:localhost",
        "$15275047031IXQRi2:localhost",
        "$098237280074GZeOm2:localhost",
    ];

    let forward_events =
        forward_stream.take(expected_forward_events.len()).collect::<Vec<SyncRoomEvent>>().await;

    for (r, e) in forward_events.into_iter().zip(expected_forward_events.iter()) {
        assert_eq!(&r.event_id().unwrap().as_str(), e);
    }

    let expected_backwards_events = vec![
        "$152037280074GZeOm:localhost",
        "$1444812213350496Caaaf:example.com",
        "$1444812213350496Cbbbf:example.com",
        "$1444812213350496Ccccf:example.com",
        "$1444812213350496Caaak:example.com",
        "$1444812213350496Cbbbk:example.com",
        "$1444812213350496Cccck:example.com",
    ];

    let backward_events = backward_stream
        .take(expected_backwards_events.len())
        .collect::<Vec<matrix_sdk::Result<SyncRoomEvent>>>()
        .await;

    for (r, e) in backward_events.into_iter().zip(expected_backwards_events.iter()) {
        assert_eq!(&r.unwrap().event_id().unwrap().as_str(), e);
    }
}

#[async_test]
#[cfg(feature = "experimental-timeline")]
async fn room_timeline() {
    use futures_util::StreamExt;
    use matrix_sdk::deserialized_responses::SyncRoomEvent;
    use wiremock::matchers::query_param;

    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    mock_sync(&server, &*test_json::MORE_SYNC, None).await;

    let _ = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();
    let (forward_stream, backward_stream) = room.timeline().await.unwrap();

    let sync_token = client.sync_token().await.unwrap();
    assert_eq!(sync_token, "s526_47314_0_7_1_1_1_11444_2");
    mock_sync(&server, &*test_json::MORE_SYNC_2, Some(sync_token.clone())).await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "t392-516_47314_0_7_1_1_1_11444_1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::SYNC_ROOM_MESSAGES_BATCH_1),
        )
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "t47409-4357353_219380_26003_2269"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(&*test_json::SYNC_ROOM_MESSAGES_BATCH_2),
        )
        .expect(1)
        .named("messages_batch_2")
        .mount(&server)
        .await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000)).token(sync_token);
    let _ = client.sync_once(sync_settings).await.unwrap();

    let expected_forward_events = vec![
        "$152037280074GZeOm2:localhost",
        "$editevid2:localhost",
        "$151957878228ssqrJ2:localhost",
        "$15275046980maRLj2:localhost",
        "$15275047031IXQRi2:localhost",
        "$098237280074GZeOm2:localhost",
    ];

    let forward_events =
        forward_stream.take(expected_forward_events.len()).collect::<Vec<SyncRoomEvent>>().await;

    for (r, e) in forward_events.into_iter().zip(expected_forward_events.iter()) {
        assert_eq!(&r.event_id().unwrap().as_str(), e);
    }

    let expected_backwards_events = vec![
        "$098237280074GZeOm:localhost",
        "$15275047031IXQRi:localhost",
        "$15275046980maRLj:localhost",
        "$151957878228ssqrJ:localhost",
        "$editevid:localhost",
        "$152037280074GZeOm:localhost",
        // ^^^ These come from the first sync before we asked for the timeline and thus
        //     where cached
        //
        // While the following are fetched over the network transparently to us after,
        // when scrolling back in time:
        "$1444812213350496Caaaf:example.com",
        "$1444812213350496Cbbbf:example.com",
        "$1444812213350496Ccccf:example.com",
        "$1444812213350496Caaak:example.com",
        "$1444812213350496Cbbbk:example.com",
        "$1444812213350496Cccck:example.com",
    ];

    let backward_events = backward_stream
        .take(expected_backwards_events.len())
        .collect::<Vec<matrix_sdk::Result<SyncRoomEvent>>>()
        .await;

    for (r, e) in backward_events.into_iter().zip(expected_backwards_events.iter()) {
        assert_eq!(&r.unwrap().event_id().unwrap().as_str(), e);
    }
}

#[async_test]
async fn room_permalink() {
    fn sync_response(index: u8, room_timeline_events: &[JsonValue]) -> JsonValue {
        json!({
            "device_one_time_keys_count": {},
            "next_batch": format!("s526_47314_0_7_1_1_1_11444_{}", index + 1),
            "device_lists": {
                "changed": [],
                "left": []
            },
            "account_data": {
                "events": []
            },
            "rooms": {
                "invite": {},
                "join": {
                    "!test_room:127.0.0.1": {
                        "summary": {},
                        "account_data": {
                            "events": []
                        },
                        "ephemeral": {
                            "events": []
                        },
                        "state": {
                            "events": []
                        },
                        "timeline": {
                            "events": room_timeline_events,
                            "limited": false,
                            "prev_batch": format!("s526_47314_0_7_1_1_1_11444_{}", index - 1),
                        },
                        "unread_notifications": {
                            "highlight_count": 0,
                            "notification_count": 0,
                        }
                    }
                },
                "leave": {}
            },
            "to_device": {
                "events": []
            },
            "presence": {
                "events": []
            }
        })
    }

    fn room_member_events(nb: usize, server: &str) -> Vec<JsonValue> {
        let mut events = Vec::with_capacity(nb);
        for i in 0..nb {
            let id = format!("${server}{i}");
            let user = format!("@user{i}:{server}");
            events.push(json!({
                "content": {
                    "membership": "join",
                },
                "event_id": id,
                "origin_server_ts": 151800140,
                "sender": user,
                "state_key": user,
                "type": "m.room.member",
            }))
        }
        events
    }

    let (client, server) = logged_in_client().await;

    // Without elligible server
    let mut sync_index = 1;
    let res = sync_response(
        sync_index,
        &[
            json!({
                "content": {
                    "creator": "@creator:127.0.0.1",
                    "room_version": "6",
                },
                "event_id": "$151957878228ekrDs",
                "origin_server_ts": 15195787,
                "sender": "@creator:localhost",
                "state_key": "",
                "type": "m.room.create",
            }),
            json!({
                "content": {
                    "membership": "join",
                },
                "event_id": "$151800140517rfvjc",
                "origin_server_ts": 151800140,
                "sender": "@creator:127.0.0.1",
                "state_key": "@creator:127.0.0.1",
                "type": "m.room.member",
            }),
        ],
    );
    mock_sync(&server, res, None).await;
    client.sync_once(SyncSettings::new()).await.unwrap();
    let room = client.get_room(room_id!("!test_room:127.0.0.1")).unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1"
    );
    assert_eq!(
        room.matrix_permalink(true).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1?action=join"
    );

    // With a single elligible server
    sync_index += 1;
    let res = sync_response(
        sync_index,
        &[json!({
            "content": {
                "membership": "join",
            },
            "event_id": "$151800140517rfvjc",
            "origin_server_ts": 151800140,
            "sender": "@example:localhost",
            "state_key": "@example:localhost",
            "type": "m.room.member",
        })],
    );
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, res, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1?via=localhost"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1?via=localhost"
    );

    // With two elligible servers
    sync_index += 1;
    let res = sync_response(sync_index, &room_member_events(15, "notarealhs"));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, res, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1?via=notarealhs&via=localhost"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1?via=notarealhs&via=localhost"
    );

    // With three elligible servers
    sync_index += 1;
    let res = sync_response(sync_index, &room_member_events(5, "mymatrix"));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, res, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1?via=notarealhs&via=mymatrix&via=localhost"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1?via=notarealhs&via=mymatrix&via=localhost"
    );

    // With four elligible servers
    sync_index += 1;
    let res = sync_response(sync_index, &room_member_events(10, "yourmatrix"));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, res, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1?via=notarealhs&via=yourmatrix&via=mymatrix"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1?via=notarealhs&via=yourmatrix&via=mymatrix"
    );

    // With power levels
    sync_index += 1;
    let res = sync_response(
        sync_index,
        &[json!({
            "content": {
                "users": {
                    "@example:localhost": 50,
                },
            },
            "event_id": "$15139375512JaHAW",
            "origin_server_ts": 151393755,
            "sender": "@creator:127.0.0.1",
            "state_key": "",
            "type": "m.room.power_levels",
        })],
    );
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, res, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1?via=localhost&via=notarealhs&via=yourmatrix"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1?via=localhost&via=notarealhs&via=yourmatrix"
    );

    // With higher power levels
    sync_index += 1;
    let res = sync_response(
        sync_index,
        &[json!({
            "content": {
                "users": {
                    "@example:localhost": 50,
                    "@user0:mymatrix": 70,
                },
            },
            "event_id": "$15139375512JaHAZ",
            "origin_server_ts": 151393755,
            "sender": "@creator:127.0.0.1",
            "state_key": "",
            "type": "m.room.power_levels",
        })],
    );
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, res, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1?via=mymatrix&via=notarealhs&via=yourmatrix"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1?via=mymatrix&via=notarealhs&via=yourmatrix"
    );

    // With server ACLs
    sync_index += 1;
    let res = sync_response(
        sync_index,
        &[json!({
            "content": {
                "allow": ["*"],
                "allow_ip_literals": true,
                "deny": ["notarealhs"],
            },
            "event_id": "$143273582443PhrSn",
            "origin_server_ts": 1432735824,
            "sender": "@creator:127.0.0.1",
            "state_key": "",
            "type": "m.room.server_acl",
        })],
    );
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, res, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1?via=mymatrix&via=yourmatrix&via=localhost"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1?via=mymatrix&via=yourmatrix&via=localhost"
    );

    // With an alternative alias
    sync_index += 1;
    let res = sync_response(
        sync_index,
        &[json!({
            "content": {
                "alt_aliases": ["#alias:localhost"],
            },
            "event_id": "$15139375513VdeRF",
            "origin_server_ts": 151393755,
            "sender": "@example:localhost",
            "state_key": "",
            "type": "m.room.canonical_alias",
        })],
    );
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, res, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%23alias%3Alocalhost"
    );
    assert_eq!(room.matrix_permalink(false).await.unwrap().to_string(), "matrix:r/alias:localhost");

    // With a canonical alias
    sync_index += 1;
    let res = sync_response(
        sync_index,
        &[json!({
            "content": {
                "alias": "#canonical:localhost",
                "alt_aliases": ["#alias:localhost"],
            },
            "event_id": "$15139375513VdeRF",
            "origin_server_ts": 151393755,
            "sender": "@example:localhost",
            "state_key": "",
            "type": "m.room.canonical_alias",
        })],
    );
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, res, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%23canonical%3Alocalhost"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:r/canonical:localhost"
    );
    assert_eq!(
        room.matrix_permalink(true).await.unwrap().to_string(),
        "matrix:r/canonical:localhost?action=join"
    );

    let event_id = event_id!("$15139375512JaHAW");
    assert_eq!(
        room.matrix_to_event_permalink(event_id).await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1/%2415139375512JaHAW?via=mymatrix&via=yourmatrix&via=localhost"
    );
    assert_eq!(
        room.matrix_event_permalink(event_id).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1/e/15139375512JaHAW?via=mymatrix&via=yourmatrix&via=localhost"
    );
}
