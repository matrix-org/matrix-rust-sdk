use std::{iter, time::Duration};

use assert_matches2::assert_let;
use js_int::uint;
use matrix_sdk::{
    config::SyncSettings, room::RoomMember, test_utils::events::EventFactory, DisplayName,
    RoomMemberships,
};
use matrix_sdk_test::{
    async_test, bulk_room_members, sync_state_event, sync_timeline_event, test_json,
    GlobalAccountDataTestEvent, JoinedRoomBuilder, LeftRoomBuilder, StateTestEvent,
    SyncResponseBuilder, BOB, DEFAULT_TEST_ROOM_ID,
};
use ruma::{
    event_id,
    events::{
        room::{member::MembershipState, message::RoomMessageEventContent},
        AnyStateEvent, AnySyncStateEvent, AnyTimelineEvent, StateEventType,
    },
    room_id,
};
use serde_json::json;
use wiremock::{
    matchers::{body_json, header, method, path, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client_with_server, mock_sync};

#[async_test]
async fn test_user_presence() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/members"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::MEMBERS))
        .mount(&server)
        .await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    let members: Vec<RoomMember> = room.members(RoomMemberships::ACTIVE).await.unwrap();

    assert_eq!(2, members.len());
    // assert!(room.power_levels.is_some())
}

#[async_test]
async fn test_calculate_room_names_from_summary() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::DEFAULT_SYNC_SUMMARY, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    assert_eq!(
        DisplayName::Calculated("example2".to_owned()),
        room.compute_display_name().await.unwrap()
    );
}

#[async_test]
async fn test_room_names() {
    let (client, server) = logged_in_client_with_server().await;
    let own_user_id = client.user_id().unwrap();

    // Room with a canonical alias.
    mock_sync(&server, &*test_json::SYNC, None).await;

    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    assert_eq!(client.rooms().len(), 1);
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    assert_eq!(
        DisplayName::Aliased("tutorial".to_owned()),
        room.compute_display_name().await.unwrap()
    );

    // Room with a name.
    mock_sync(&server, &*test_json::INVITE_SYNC, None).await;

    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    assert_eq!(client.rooms().len(), 2);
    let invited_room = client.get_room(room_id!("!696r7674:example.com")).unwrap();

    assert_eq!(
        DisplayName::Named("My Room Name".to_owned()),
        invited_room.compute_display_name().await.unwrap()
    );

    let mut sync_builder = SyncResponseBuilder::new();

    let own_left_member_event = sync_state_event!({
        "content": {
            "membership": "leave",
        },
        "event_id": "$747273582443PhrS9:localhost",
        "origin_server_ts": 1472735820,
        "sender": own_user_id,
        "state_key": own_user_id,
        "type": "m.room.member",
        "unsigned": {
            "age": 1234
        }
    });

    // Left room with a lot of members.
    let room_id = room_id!("!plenty_of_members:localhost");
    sync_builder.add_left_room(
        LeftRoomBuilder::new(room_id).add_state_bulk(
            bulk_room_members(0, 0..15, "localhost", &MembershipState::Join)
                .chain(iter::once(own_left_member_event.clone())),
        ),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();

    assert_eq!(
        DisplayName::Calculated(
            "user_0, user_1, user_10, user_11, user_12, and 10 others".to_owned()
        ),
        room.compute_display_name().await.unwrap()
    );

    // Room with joined and invited members.
    let room_id = room_id!("!joined_invited_members:localhost");
    sync_builder.add_left_room(LeftRoomBuilder::new(room_id).add_state_bulk([
        sync_state_event!({
            "content": {
                "membership": "join",
            },
            "event_id": "$example1_join",
            "origin_server_ts": 151800140,
            "sender": "@example1:localhost",
            "state_key": "@example1:localhost",
            "type": "m.room.member",
        }),
        sync_state_event!({
            "content": {
                "displayname": "Bob",
                "membership": "invite",
            },
            "event_id": "$bob_invite",
            "origin_server_ts": 151800140,
            "sender": "@example1:localhost",
            "state_key": "@bob:localhost",
            "type": "m.room.member",
        }),
        sync_state_event!({
            "content": {
                "membership": "leave",
            },
            "event_id": "$example3_leave",
            "origin_server_ts": 151800140,
            "sender": "@example3:localhost",
            "state_key": "@example3:localhost",
            "type": "m.room.member",
        }),
        own_left_member_event.clone(),
    ]));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();

    assert_eq!(
        DisplayName::Calculated("Bob, example1".to_owned()),
        room.compute_display_name().await.unwrap()
    );

    // Room with only left members.
    let room_id = room_id!("!left_members:localhost");
    sync_builder.add_left_room(
        LeftRoomBuilder::new(room_id).add_state_bulk(
            bulk_room_members(0, 0..3, "localhost", &MembershipState::Leave)
                .chain(iter::once(own_left_member_event)),
        ),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();

    assert_eq!(
        DisplayName::EmptyWas("user_0, user_1, user_2".to_owned()),
        room.compute_display_name().await.unwrap()
    );
}

#[async_test]
async fn test_state_event_getting() {
    let (client, server) = logged_in_client_with_server().await;

    let sync = json!({
        "next_batch": "1234",
        "rooms": {
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
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

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID);
    assert!(room.is_none());

    client.sync_once(SyncSettings::default()).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

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
async fn test_room_route() {
    let (client, server) = logged_in_client_with_server().await;
    let mut sync_builder = SyncResponseBuilder::new();
    let room_id = &*DEFAULT_TEST_ROOM_ID;

    // Without eligible server
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "creator": "@creator:127.0.0.1",
                    "room_version": "6",
                },
                "event_id": "$151957878228ekrDs",
                "origin_server_ts": 15195787,
                "sender": "@creator:127.0.0.1",
                "state_key": "",
                "type": "m.room.create",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "membership": "join",
                },
                "event_id": "$151800140517rfvjc",
                "origin_server_ts": 151800140,
                "sender": "@creator:127.0.0.1",
                "state_key": "@creator:127.0.0.1",
                "type": "m.room.member",
            })),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let sync_token = client.sync_once(SyncSettings::new()).await.unwrap().next_batch;
    let room = client.get_room(room_id).unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 0);

    // With a single eligible server
    let mut batch = 0;
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..1, "localhost", &MembershipState::Join),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 1);
    assert_eq!(route[0], "localhost");

    // With two eligible servers
    batch += 1;
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..15, "notarealhs", &MembershipState::Join),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 2);
    assert_eq!(route[0], "notarealhs");
    assert_eq!(route[1], "localhost");

    // With three eligible servers
    batch += 1;
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..5, "mymatrix", &MembershipState::Join),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "notarealhs");
    assert_eq!(route[1], "mymatrix");
    assert_eq!(route[2], "localhost");

    // With four eligible servers
    batch += 1;
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_state_bulk(
        bulk_room_members(batch, 0..10, "yourmatrix", &MembershipState::Join),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "notarealhs");
    assert_eq!(route[1], "yourmatrix");
    assert_eq!(route[2], "mymatrix");

    // With power levels
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
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
        }),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "localhost");
    assert_eq!(route[1], "notarealhs");
    assert_eq!(route[2], "yourmatrix");

    // With higher power levels
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
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
        }),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "mymatrix");
    assert_eq!(route[1], "notarealhs");
    assert_eq!(route[2], "yourmatrix");

    // With server ACLs
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
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
        }),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap();

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 3);
    assert_eq!(route[0], "mymatrix");
    assert_eq!(route[1], "yourmatrix");
    assert_eq!(route[2], "localhost");
}

#[async_test]
async fn test_room_permalink() {
    let (client, server) = logged_in_client_with_server().await;
    let mut sync_builder = SyncResponseBuilder::new();
    let room_id = room_id!("!test_room:127.0.0.1");

    // Without aliases
    sync_builder.add_joined_room(
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
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
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
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "alt_aliases": ["#alias:localhost"],
            },
            "event_id": "$15139375513VdeRF",
            "origin_server_ts": 151393755,
            "sender": "@user_0:localhost",
            "state_key": "",
            "type": "m.room.canonical_alias",
        }),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
    let sync_token =
        client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch;

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        "https://matrix.to/#/%23alias:localhost"
    );
    assert_eq!(room.matrix_permalink(false).await.unwrap().to_string(), "matrix:r/alias:localhost");

    // With a canonical alias
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "alias": "#canonical:localhost",
                "alt_aliases": ["#alias:localhost"],
            },
            "event_id": "$15139375513VdeRF",
            "origin_server_ts": 151393755,
            "sender": "@user_0:localhost",
            "state_key": "",
            "type": "m.room.canonical_alias",
        }),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
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
async fn test_room_event_permalink() {
    let (client, server) = logged_in_client_with_server().await;
    let mut sync_builder = SyncResponseBuilder::new();
    let room_id = room_id!("!test_room:127.0.0.1");
    let event_id = event_id!("$15139375512JaHAW");

    // Without aliases
    sync_builder.add_joined_room(
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
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
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
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "alias": "#canonical:localhost",
                "alt_aliases": ["#alias:localhost"],
            },
            "event_id": "$15139375513VdeRF",
            "origin_server_ts": 151393755,
            "sender": "@user_0:localhost",
            "state_key": "",
            "type": "m.room.canonical_alias",
        }),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), Some(sync_token.clone())).await;
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
async fn test_event() {
    let event_id = event_id!("$foun39djjod0f");

    let (client, server) = logged_in_client_with_server().await;
    let cache = client.event_cache();
    let _ = cache.subscribe();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder
        // We need the member event and power levels locally so the push rules processor works.
        .add_joined_room(
            JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
                .add_state_event(StateTestEvent::Member)
                .add_state_event(StateTestEvent::PowerLevels),
        );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

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
        "room_id": *DEFAULT_TEST_ROOM_ID,
    });
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_json))
        .expect(1)
        .named("event_1")
        .mount(&server)
        .await;

    let timeline_event = room.event(event_id, None).await.unwrap();
    assert_let!(
        AnyTimelineEvent::State(AnyStateEvent::RoomTombstone(event)) =
            timeline_event.event.deserialize().unwrap()
    );
    assert_eq!(event.event_id(), event_id);

    let push_actions = timeline_event.push_actions.unwrap();
    assert!(push_actions.iter().any(|a| a.is_highlight()));
    assert!(push_actions.iter().any(|a| a.should_notify()));

    // Requested event was saved to the cache
    assert!(cache.event(event_id).await.is_some());
}

#[async_test]
async fn test_event_with_context() {
    let event_id = event_id!("$cur1234");
    let prev_event_id = event_id!("$prev1234");
    let next_event_id = event_id!("$next_1234");

    let (client, server) = logged_in_client_with_server().await;
    let cache = client.event_cache();
    let _ = cache.subscribe();

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder
        // We need the member event and power levels locally so the push rules processor works.
        .add_joined_room(
            JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
                .add_state_event(StateTestEvent::Member)
                .add_state_event(StateTestEvent::PowerLevels),
        );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    let room_id = room.room_id();

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let event =
        f.event(RoomMessageEventContent::text_plain("The requested message")).event_id(event_id);
    let event_before =
        f.event(RoomMessageEventContent::text_plain("A previous message")).event_id(prev_event_id);
    let event_next =
        f.event(RoomMessageEventContent::text_plain("A newer message")).event_id(next_event_id);
    Mock::given(method("GET"))
        .and(path(format!("/_matrix/client/r0/rooms/{room_id}/context/{event_id}")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "events_before": [event_before.into_raw_timeline()],
            "event": event.into_raw_timeline(),
            "events_after": [event_next.into_raw_timeline()],
            "state": [],
        })))
        .mount(&server)
        .await;

    let context_ret = room.event_with_context(event_id, false, uint!(1), None).await.unwrap();

    assert_let!(Some(timeline_event) = context_ret.event);
    assert_let!(Ok(event) = timeline_event.event.deserialize());
    assert_eq!(event.event_id(), event_id);

    assert_eq!(1, context_ret.events_before.len());
    assert_let!(Ok(event) = context_ret.events_before[0].event.deserialize());
    assert_eq!(event.event_id(), prev_event_id);

    assert_eq!(1, context_ret.events_after.len());
    assert_let!(Ok(event) = context_ret.events_after[0].event.deserialize());
    assert_eq!(event.event_id(), next_event_id);

    // Requested event and their context ones were saved to the cache
    assert!(cache.event(event_id).await.is_some());
    assert!(cache.event(prev_event_id).await.is_some());
    assert!(cache.event(next_event_id).await.is_some());
}

#[async_test]
async fn test_is_direct() {
    let (client, server) = logged_in_client_with_server().await;
    let own_user_id = client.user_id().unwrap();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let bob_member_event = json!({
        "content": {
            "membership": "join",
        },
        "event_id": "$747273582443PhrSn:localhost",
        "origin_server_ts": 1472735824,
        "sender": *BOB,
        "state_key": *BOB,
        "type": "m.room.member",
        "unsigned": {
            "age": 1234
        }
    });

    // Initialize the room with 2 members, including ourself.
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID)
            .add_state_event(StateTestEvent::Member)
            .add_state_event(StateTestEvent::Custom(bob_member_event.clone())),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    // The room is not direct.
    assert!(room.direct_targets().is_empty());
    assert!(!room.is_direct().await.unwrap());

    // Set the room as direct.
    let direct_content = json!({
        *BOB: [*DEFAULT_TEST_ROOM_ID],
    });

    // Setting the room as direct will request the members of the room.
    Mock::given(method("GET"))
        .and(path_regex(format!("^/_matrix/client/r0/rooms/{}/members$", *DEFAULT_TEST_ROOM_ID)))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": [
                *test_json::MEMBER,
                bob_member_event,
            ],
        })))
        .expect(1)
        .named("members")
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path_regex(format!("^/_matrix/client/r0/user/{own_user_id}/account_data/m.direct$")))
        .and(header("authorization", "Bearer 1234"))
        .and(body_json(&direct_content))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("set_direct")
        .mount(&server)
        .await;

    room.set_is_direct(true).await.unwrap();

    // Mock the sync response we should get from the homeserver.
    sync_builder.add_global_account_data_event(GlobalAccountDataTestEvent::Custom(json!({
        "type": "m.direct",
        "content": direct_content,
    })));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // The room is direct now.
    let direct_targets = room.direct_targets();
    assert_eq!(direct_targets.len(), 1);
    assert!(direct_targets.contains(*BOB));
    assert!(room.is_direct().await.unwrap());

    // Unset the room as direct.
    let direct_content = json!({});

    Mock::given(method("PUT"))
        .and(path_regex(format!("^/_matrix/client/r0/user/{own_user_id}/account_data/m.direct$")))
        .and(header("authorization", "Bearer 1234"))
        .and(body_json(&direct_content))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("unset_direct")
        .mount(&server)
        .await;

    // Mock the sync response we should get from the homeserver.
    sync_builder.add_global_account_data_event(GlobalAccountDataTestEvent::Custom(json!({
        "type": "m.direct",
        "content": direct_content,
    })));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // The room is not direct anymore.
    assert!(room.direct_targets().is_empty());
    assert!(!room.is_direct().await.unwrap());
}
