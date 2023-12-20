use std::time::Duration;

use assert_matches2::assert_let;
use futures_util::StreamExt;
use matrix_sdk::{config::SyncSettings, room::ParentSpace, Client};
use matrix_sdk_test::{async_test, test_json, DEFAULT_TEST_ROOM_ID};
use once_cell::sync::Lazy;
use ruma::{room_id, RoomId};
use serde_json::{json, Value as JsonValue};
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync, MockServer};

pub static DEFAULT_TEST_SPACE_ID: Lazy<&RoomId> =
    Lazy::new(|| room_id!("!hIMjEx205EXNyjVPCV:localhost"));

pub static PARENT_SPACE_SYNC: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "device_one_time_keys_count": {},
        "next_batch": "s526_47314_0_7_1_1_1_11444_2",
        "device_lists": {
            "changed": [
                "@example:example.org"
            ],
            "left": []
        },
        "rooms": {
            "invite": {},
            "join": {
                *DEFAULT_TEST_ROOM_ID: {
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
                        "events": [
                            {
                                "content": {
                                    "canonical": true,
                                    "via": [
                                        "example.org",
                                        "other.example.org"
                                    ]
                                },
                                "event_id": "$143273582443PhrSn:localhost",
                                "origin_server_ts": 152037280000000_u64,
                                "sender": "@spaceadmin:localhost",
                                "state_key": *DEFAULT_TEST_SPACE_ID,
                                "type": "m.space.parent",
                                "unsigned": {
                                    "age": 598971425
                                }
                            },
                        ],
                        "limited": false,
                        "prev_batch": "t392-516_47314_0_7_1_1_1_11444_1"
                    },
                    "unread_notifications": {
                        "highlight_count": 0,
                        "notification_count": 11
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
});

async fn mock_members(server: &MockServer) {
    let members = json!({
        "chunk": [
            {
                "content": {
                    "avatar_url": null,
                    "displayname": "Space Administrator",
                    "membership": "join"
                },
                "event_id": "$151800140517rfvjc:localhost",
                "membership": "join",
                "origin_server_ts": 151800140,
                "room_id": *DEFAULT_TEST_SPACE_ID,
                "sender": "@spaceadmin:localhost",
                "state_key": "@spaceadmin:localhost",
                "type": "m.room.member",
                "unsigned": {
                    "age": 2970366,
                }
            }
        ]
    });
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/members"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(members))
        .mount(server)
        .await;
}

/// Performs an initial sync with sample data, then with the provided `sync`.
/// Returns the next sync token.
async fn initial_sync_with_m_space_parent(
    client: &Client,
    server: &MockServer,
    sync: &JsonValue,
) -> String {
    mock_sync(server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let sync_token = client.sync_once(sync_settings).await.unwrap().next_batch;

    mock_sync(server, sync, Some(sync_token.clone())).await;

    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch
}

/// Syncs with a parent space, using the previous `sync_token` and including any
/// custom `state_events` for the current test.
///
/// Returns the next sync token.
async fn sync_space(
    client: &Client,
    server: &MockServer,
    sync_token: String,
    state_events: Vec<JsonValue>,
) -> String {
    // synthetize a summary for the space by using a sample summary and replacing
    // the room id
    let mut parent_sync = test_json::DEFAULT_SYNC_SUMMARY.clone();
    let join = parent_sync["rooms"]["join"].as_object_mut().unwrap();
    let mut timeline = join.remove(DEFAULT_TEST_ROOM_ID.as_str()).unwrap();
    timeline["state"]["events"].as_array_mut().unwrap().extend(state_events); // add custom events
    join.insert(DEFAULT_TEST_SPACE_ID.to_string(), timeline);

    mock_sync(server, parent_sync, Some(sync_token.clone())).await;
    client.sync_once(SyncSettings::new().token(sync_token)).await.unwrap().next_batch
}

#[async_test]
async fn no_parent_space() {
    let (client, server) = logged_in_client().await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 0);
}

#[async_test]
async fn parent_space_undeserializable() {
    let (client, server) = logged_in_client().await;

    let mut sync = PARENT_SPACE_SYNC.clone();
    sync["rooms"]["join"][DEFAULT_TEST_ROOM_ID.as_str()]["timeline"]["events"][0]["content"]
        ["canonical"] = JsonValue::from("true"); // invalid, must be a boolean
    initial_sync_with_m_space_parent(&client, &server, &sync).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 0);
}

#[async_test]
async fn parent_space_redacted() {
    let (client, server) = logged_in_client().await;

    let mut sync = PARENT_SPACE_SYNC.clone();
    let timeline = &mut sync["rooms"]["join"][DEFAULT_TEST_ROOM_ID.as_str()]["timeline"]["events"];
    let event_id = timeline[0]["event_id"].clone();
    timeline.as_array_mut().unwrap().push(json!({
        "content": {
            "reason": "test"
        },
        "event_id": "$151957878228ssqrJ2:localhost",
        "origin_server_ts": 151957878000000_u64,
        "sender": "@spaceadmin:localhost",
        "type": "m.room.redaction",
        "redacts": event_id,
        "unsigned": {
            "age": 85
        }
    }));
    initial_sync_with_m_space_parent(&client, &server, &sync).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 0);
}

#[async_test]
async fn parent_space_unverifiable() {
    let (client, server) = logged_in_client().await;

    initial_sync_with_m_space_parent(&client, &server, &PARENT_SPACE_SYNC).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Unverifiable(space_id) = spaces.first().unwrap());
    assert_eq!(space_id, *DEFAULT_TEST_SPACE_ID);
}

#[async_test]
async fn parent_space_illegitimate() {
    let (client, server) = logged_in_client().await;

    mock_members(&server).await;

    let sync_token = initial_sync_with_m_space_parent(&client, &server, &PARENT_SPACE_SYNC).await;

    sync_space(&client, &server, sync_token, vec![]).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Illegitimate(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), *DEFAULT_TEST_SPACE_ID);
}

#[async_test]
async fn parent_space_reciprocal() {
    let (client, server) = logged_in_client().await;

    let sync_token = initial_sync_with_m_space_parent(&client, &server, &PARENT_SPACE_SYNC).await;

    let child_event = json!({
        "content": {
            "suggested": true,
            "via": [
                "example.org",
                "other.example.org"
            ]
        },
        "event_id": "$143273582443PhrSn:example.org",
        "origin_server_ts": 1432735824653_u64,
        "room_id": *DEFAULT_TEST_SPACE_ID,
        "sender": "@example2:example.org", // Not equal to sender of m.room.space because
                                           // equality is only required for power-levels
        "state_key": *DEFAULT_TEST_ROOM_ID,
        "type": "m.space.child",
        "unsigned": {
            "age": 1234
        }
    });

    sync_space(&client, &server, sync_token, vec![child_event]).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Reciprocal(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), *DEFAULT_TEST_SPACE_ID);
}

#[async_test]
async fn parent_space_redacted_reciprocal() {
    let (client, server) = logged_in_client().await;

    mock_members(&server).await;

    let sync_token = initial_sync_with_m_space_parent(&client, &server, &PARENT_SPACE_SYNC).await;

    let child_event = json!({
        "content": {},  // Redacted -> missing "via" key -> invalidates the relationship
        "event_id": "$143273582443PhrSn:example.org",
        "origin_server_ts": 1432735824653_u64,
        "room_id": *DEFAULT_TEST_SPACE_ID,
        "sender": "@example2:example.org",
        "state_key": *DEFAULT_TEST_ROOM_ID,
        "type": "m.space.child",
        "unsigned": {
            "age": 1234,
            "redacted_because": {
                "content": {},
                "event_id": "$15275047031IXQRi:localhost",
                "origin_server_ts": 152750470000000_u64,
                "redacts": "$15275046980maRLj:localhost",
                "sender": "@example:localhost",
                "type": "m.room.redaction",
                "unsigned": {
                    "age": 14523
                }
            },
        }
    });

    sync_space(&client, &server, sync_token, vec![child_event]).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Illegitimate(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), *DEFAULT_TEST_SPACE_ID);
}

/// Adds @spaceadmin:localhost to the room with the given power level
async fn setup_parent_member(
    client: &Client,
    server: &MockServer,
    sync_token: String,
    level: u64,
) -> String {
    mock_members(server).await;

    let pl_event = json!({
        "content": {
              "ban": 50,
              "events": {
                  "m.room.avatar": 50,
                  "m.room.canonical_alias": 50,
                  "m.room.history_visibility": 100,
                  "m.room.name": 50,
                  "m.room.power_levels": 100,
                  "m.space.child": 2,
              },
              "events_default": 0,
              "invite": 0,
              "kick": 50,
              "redact": 50,
              "state_default": 50,
              "users": {
                  "@example:localhost": 100,
                  "@spaceadmin:localhost": level  // sender of m.space.parent in child room
              },
              "users_default": 0
        },
        "event_id": "$143273582443PhrSn:example.org",
        "origin_server_ts": 1432735824653_u64,
        "room_id": *DEFAULT_TEST_SPACE_ID,
        "sender": "@example2:example.org",
        "state_key": "",
        "type": "m.room.power_levels",
        "unsigned": {
            "age": 1234
        }
    });

    sync_space(client, server, sync_token, vec![pl_event]).await
}

#[async_test]
async fn parent_space_powerlevel() {
    let (client, server) = logged_in_client().await;

    let sync_token = initial_sync_with_m_space_parent(&client, &server, &PARENT_SPACE_SYNC).await;

    setup_parent_member(&client, &server, sync_token, 2).await; // >= PL for m.room.child

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::WithPowerlevel(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), *DEFAULT_TEST_SPACE_ID);
}

#[async_test]
async fn parent_space_powerlevel_too_low() {
    let (client, server) = logged_in_client().await;

    let sync_token = initial_sync_with_m_space_parent(&client, &server, &PARENT_SPACE_SYNC).await;

    setup_parent_member(&client, &server, sync_token, 1).await; // < PL for m.room.child

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let spaces: Vec<ParentSpace> =
        room.parent_spaces().await.unwrap().map(Result::unwrap).collect().await;
    assert_eq!(spaces.len(), 1);
    assert_let!(ParentSpace::Illegitimate(space) = spaces.first().unwrap());
    assert_eq!(space.room_id(), *DEFAULT_TEST_SPACE_ID);
}
