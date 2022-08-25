use std::time::Duration;

use matrix_sdk::{config::SyncSettings, DisplayName, RoomMember};
use matrix_sdk_test::{
    async_test, bulk_room_members, test_json, EventBuilder, JoinedRoomBuilder, TimelineTestEvent,
};
use ruma::{
    event_id,
    events::{room::member::MembershipState, AnySyncStateEvent, StateEventType},
    room_id,
};
use serde_json::json;
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
    use matrix_sdk::deserialized_responses::SyncTimelineEvent;
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
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_MESSAGES_BATCH_1))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "t47409-4357353_219380_26003_2269"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_MESSAGES_BATCH_2))
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

    let forward_events = forward_stream
        .take(expected_forward_events.len())
        .collect::<Vec<SyncTimelineEvent>>()
        .await;

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
        .collect::<Vec<matrix_sdk::Result<SyncTimelineEvent>>>()
        .await;

    for (r, e) in backward_events.into_iter().zip(expected_backwards_events.iter()) {
        assert_eq!(&r.unwrap().event_id().unwrap().as_str(), e);
    }
}

#[async_test]
#[cfg(feature = "experimental-timeline")]
async fn room_timeline() {
    use futures_util::StreamExt;
    use matrix_sdk::deserialized_responses::SyncTimelineEvent;
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
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_MESSAGES_BATCH_1))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "t47409-4357353_219380_26003_2269"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_MESSAGES_BATCH_2))
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

    let forward_events = forward_stream
        .take(expected_forward_events.len())
        .collect::<Vec<SyncTimelineEvent>>()
        .await;

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
        .collect::<Vec<matrix_sdk::Result<SyncTimelineEvent>>>()
        .await;

    for (r, e) in backward_events.into_iter().zip(expected_backwards_events.iter()) {
        assert_eq!(&r.unwrap().event_id().unwrap().as_str(), e);
    }
}

#[async_test]
async fn room_route() {
    let (client, server) = logged_in_client().await;
    let mut ev_builder = EventBuilder::new();
    let room_id = room_id!("!test_room:127.0.0.1");

    // Without elligible server
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {
                    "creator": "@creator:127.0.0.1",
                    "room_version": "6",
                },
                "event_id": "$151957878228ekrDs",
                "origin_server_ts": 15195787,
                "sender": "@creator:127.0.0.1",
                "state_key": "",
                "type": "m.room.create",
            })))
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {
                    "membership": "join",
                },
                "event_id": "$151800140517rfvjc",
                "origin_server_ts": 151800140,
                "sender": "@creator:127.0.0.1",
                "state_key": "@creator:127.0.0.1",
                "type": "m.room.member",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::new()).await.unwrap();
    let room = client.get_room(room_id).unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 0);

    // With a single elligible server
    let mut batch = 0;
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..1, "localhost", &MembershipState::Join),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0], "localhost");

    // With two elligible servers
    batch += 1;
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..15, "notarealhs", &MembershipState::Join),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 2);
    assert_eq!(route[0], "notarealhs");
    assert_eq!(route[1], "localhost");

    // With three elligible servers
    batch += 1;
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..5, "mymatrix", &MembershipState::Join),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "notarealhs");
    assert_eq!(route[1], "mymatrix");
    assert_eq!(route[2], "localhost");

    // With four elligible servers
    batch += 1;
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..10, "yourmatrix", &MembershipState::Join),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "notarealhs");
    assert_eq!(route[1], "yourmatrix");
    assert_eq!(route[2], "mymatrix");

    // With power levels
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "users": {
                    "@user_0:localhost": 50,
                },
            },
            "event_id": "$15139375512JaHAW",
            "origin_server_ts": 151393755,
            "sender": "@creator:127.0.0.1",
            "state_key": "",
            "type": "m.room.power_levels",
        })),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "localhost");
    assert_eq!(route[1], "notarealhs");
    assert_eq!(route[2], "yourmatrix");

    // With higher power levels
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "users": {
                    "@user_0:localhost": 50,
                    "@user_2:mymatrix": 70,
                },
            },
            "event_id": "$15139375512JaHAZ",
            "origin_server_ts": 151393755,
            "sender": "@creator:127.0.0.1",
            "state_key": "",
            "type": "m.room.power_levels",
        })),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "mymatrix");
    assert_eq!(route[1], "notarealhs");
    assert_eq!(route[2], "yourmatrix");

    // With server ACLs
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
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
        })),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "mymatrix");
    assert_eq!(route[1], "yourmatrix");
    assert_eq!(route[2], "localhost");
}

#[async_test]
async fn room_permalink() {
    let (client, server) = logged_in_client().await;
    let mut ev_builder = EventBuilder::new();
    let room_id = room_id!("!test_room:127.0.0.1");

    // Without aliases
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_state_bulk(bulk_room_members(
                0,
                0..1,
                "localhost",
                &MembershipState::Join,
            ))
            .add_timeline_state_bulk(bulk_room_members(
                1,
                0..5,
                "notarealhs",
                &MembershipState::Join,
            )),
    );
    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::new()).await.unwrap();
    let room = client.get_room(room_id).unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1?via=notarealhs&via=localhost"
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1?via=notarealhs&via=localhost"
    );

    // With an alternative alias
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "alt_aliases": ["#alias:localhost"],
            },
            "event_id": "$15139375513VdeRF",
            "origin_server_ts": 151393755,
            "sender": "@user_0:localhost",
            "state_key": "",
            "type": "m.room.canonical_alias",
        })),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%23alias%3Alocalhost"
    );
    assert_eq!(room.matrix_permalink(false).await.unwrap().to_string(), "matrix:r/alias:localhost");

    // With a canonical alias
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "alias": "#canonical:localhost",
                "alt_aliases": ["#alias:localhost"],
            },
            "event_id": "$15139375513VdeRF",
            "origin_server_ts": 151393755,
            "sender": "@user_0:localhost",
            "state_key": "",
            "type": "m.room.canonical_alias",
        })),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
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
}

#[async_test]
async fn room_event_permalink() {
    let (client, server) = logged_in_client().await;
    let mut ev_builder = EventBuilder::new();
    let room_id = room_id!("!test_room:127.0.0.1");
    let event_id = event_id!("$15139375512JaHAW");

    // Without aliases
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_state_bulk(bulk_room_members(
                0,
                0..1,
                "localhost",
                &MembershipState::Join,
            ))
            .add_timeline_state_bulk(bulk_room_members(
                1,
                0..5,
                "notarealhs",
                &MembershipState::Join,
            )),
    );
    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::new()).await.unwrap();
    let room = client.get_room(room_id).unwrap();

    assert_eq!(
        room.matrix_to_event_permalink(event_id).await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1/%2415139375512JaHAW?via=notarealhs&via=localhost"
    );
    assert_eq!(
        room.matrix_event_permalink(event_id).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1/e/15139375512JaHAW?via=notarealhs&via=localhost"
    );

    // Adding an alias doesn't change anything
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "alias": "#canonical:localhost",
                "alt_aliases": ["#alias:localhost"],
            },
            "event_id": "$15139375513VdeRF",
            "origin_server_ts": 151393755,
            "sender": "@user_0:localhost",
            "state_key": "",
            "type": "m.room.canonical_alias",
        })),
    ));
    let sync_token = client.sync_token().await.unwrap();
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_event_permalink(event_id).await.unwrap().to_string(),
        "https://matrix.to/#/%21test_room%3A127.0.0.1/%2415139375512JaHAW?via=notarealhs&via=localhost"
    );
    assert_eq!(
        room.matrix_event_permalink(event_id).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1/e/15139375512JaHAW?via=notarealhs&via=localhost"
    );
}
