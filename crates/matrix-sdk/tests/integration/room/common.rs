use std::time::Duration;

use assert_matches::assert_matches;
use matrix_sdk::{config::SyncSettings, room::RoomMember, DisplayName, RoomMemberships};
use matrix_sdk_test::{
    async_test, bulk_room_members, test_json, JoinedRoomBuilder, StateTestEvent,
    SyncResponseBuilder, TimelineTestEvent,
};
use ruma::{
    event_id,
    events::{
        room::member::MembershipState, AnyStateEvent, AnySyncStateEvent, AnyTimelineEvent,
        StateEventType,
    },
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
    let members: Vec<RoomMember> = room.members(RoomMemberships::ACTIVE).await.unwrap();

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

    let sync_token = client.sync_once(sync_settings).await.unwrap().next_batch;

    assert_eq!(client.rooms().len(), 1);
    let room = client.get_joined_room(&test_json::DEFAULT_SYNC_ROOM_ID).unwrap();

    assert_eq!(DisplayName::Aliased("tutorial".to_owned()), room.display_name().await.unwrap());

    mock_sync(&server, &*test_json::INVITE_SYNC, Some(sync_token.clone())).await;

    let _response = client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(client.rooms().len(), 2);
    let invited_room = client.get_room(room_id!("!696r7674:example.com")).unwrap();

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

    assert_matches::assert_matches!(
        encryption_event.as_sync(),
        Some(AnySyncStateEvent::RoomEncryption(_))
    );
}

#[async_test]
async fn room_route() {
    let (client, server) = logged_in_client().await;
    let mut ev_builder = SyncResponseBuilder::new();
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
    let sync_token = client.sync_once(SyncSettings::new()).await.unwrap().next_batch;
    let room = client.get_room(room_id).unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 0);

    // With a single elligible server
    let mut batch = 0;
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..1, "localhost", &MembershipState::Join),
    ));
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0], "localhost");

    // With two elligible servers
    batch += 1;
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..15, "notarealhs", &MembershipState::Join),
    ));
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 2);
    assert_eq!(route[0], "notarealhs");
    assert_eq!(route[1], "localhost");

    // With three elligible servers
    batch += 1;
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..5, "mymatrix", &MembershipState::Join),
    ));
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

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
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

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
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

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
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

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
    let mut ev_builder = SyncResponseBuilder::new();
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
    let sync_token = client.sync_once(SyncSettings::new()).await.unwrap().next_batch;
    let room = client.get_room(room_id).unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/!test_room:127.0.0.1?via=notarealhs&via=localhost"
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
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%23alias:localhost"
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
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%23canonical:localhost"
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
    let mut ev_builder = SyncResponseBuilder::new();
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
    let sync_token = client.sync_once(SyncSettings::new()).await.unwrap().next_batch;
    let room = client.get_room(room_id).unwrap();

    assert_eq!(
        room.matrix_to_event_permalink(event_id).await.unwrap().to_string(),
        "https://matrix.to/#/!test_room:127.0.0.1/$15139375512JaHAW?via=notarealhs&via=localhost"
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
    mock_sync(&server, ev_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    assert_eq!(
        room.matrix_to_event_permalink(event_id).await.unwrap().to_string(),
        "https://matrix.to/#/!test_room:127.0.0.1/$15139375512JaHAW?via=notarealhs&via=localhost"
    );
    assert_eq!(
        room.matrix_event_permalink(event_id).await.unwrap().to_string(),
        "matrix:roomid/test_room:127.0.0.1/e/15139375512JaHAW?via=notarealhs&via=localhost"
    );
}

#[async_test]
async fn event() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let event_id = event_id!("$foun39djjod0f");

    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder
        // We need the member event and power levels locally so the push rules processor works.
        .add_joined_room(
            JoinedRoomBuilder::new(room_id)
                .add_state_event(StateTestEvent::Member)
                .add_state_event(StateTestEvent::PowerLevels),
        );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();

    let response_json = json!({
        "content": {
            "body": "This room has been replaced",
            "replacement_room": "!newroom:localhost",
        },
        "event_id": event_id,
        "origin_server_ts": 152039280,
        "sender": "@bob:localhost",
        "state_key": "",
        "type": "m.room.tombstone",
        "room_id": room_id,
    });
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_json))
        .expect(1)
        .named("event_1")
        .mount(&server)
        .await;

    let timeline_event = room.event(event_id).await.unwrap();
    let event = assert_matches!(
        timeline_event.event.deserialize().unwrap(),
        AnyTimelineEvent::State(AnyStateEvent::RoomTombstone(event)) => event
    );
    assert_eq!(event.event_id(), event_id);

    let push_actions = timeline_event.push_actions.unwrap();
    assert!(push_actions.iter().any(|a| a.is_highlight()));
    assert!(push_actions.iter().any(|a| a.should_notify()));
}
