use std::{sync::Mutex, time::Duration};

use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{async_test, JoinedRoomBuilder, SyncResponseBuilder, TimelineTestEvent};
use matrix_sdk_ui::notification_client::{
    NotificationClient, NotificationEvent, NotificationStatus,
};
use ruma::{event_id, events::TimelineEventType, room_id, user_id};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path},
    Mock, Request, ResponseTemplate,
};

use crate::{
    encryption_sync::check_requests,
    logged_in_client, mock_encryption_state, mock_sync,
    sliding_sync::{PartialSlidingSyncRequest, SlidingSyncMatcher},
};

#[async_test]
async fn test_notification_client_with_context() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let event_id = event_id!("$example_event_id");
    let sender = user_id!("@user:example.org");
    let event_json = json!({
        "content": {
            "body": "Hello world!",
            "msgtype": "m.text",
        },
        "room_id": room_id,
        "event_id": event_id,
        "origin_server_ts": 152049794,
        "sender": sender,
        "type": "m.room.message",
    });

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(TimelineTestEvent::Custom(event_json.clone())),
    );

    // First, mock a sync that contains a text message.
    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Then, try to simulate receiving a notification for that message.
    let notification_client = NotificationClient::builder(client).await.unwrap().build();

    {
        // The notification client retrieves the event via `/rooms/*/context/`.
        Mock::given(method("GET"))
            .and(path(format!("/_matrix/client/r0/rooms/{room_id}/context/{event_id}")))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event": event_json,
                "state": [
                    {
                        "content": {
                            "avatar_url": "https://example.org/avatar.jpeg",
                            "displayname": "John Mastodon",
                            "membership": "join"
                        },
                        "room_id": room_id,
                        "event_id": "$151800140517rfvjc:example.org",
                        "membership": "join",
                        "origin_server_ts": 151800140,
                        "sender": sender,
                        "state_key": sender,
                        "type": "m.room.member",
                        "unsigned": {
                            "age": 2970366,
                        }
                    }
                ]
            })))
            .mount(&server)
            .await;

        // The encryption state is also fetched to figure whether the room is encrypted
        // or not.
        mock_encryption_state(&server, false).await;
    }

    let item = notification_client.get_notification_with_context(room_id, event_id).await.unwrap();

    server.reset().await;

    let item = item.expect("the notification should be found");

    assert_matches::assert_matches!(item.event, NotificationEvent::Timeline(event) => {
        assert_eq!(event.event_type(), TimelineEventType::RoomMessage);
    });
    assert_eq!(item.sender_display_name.as_deref(), Some("John Mastodon"));
    assert_eq!(item.sender_avatar_url.as_deref(), Some("https://example.org/avatar.jpeg"));
}

#[async_test]
async fn test_notification_client_sliding_sync() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;

    let event_id = event_id!("$example_event_id");
    let sender = user_id!("@user:example.org");
    let my_user_id = client.user_id().unwrap().to_owned();
    let event_json = json!({
        "content": {
            "body": "Hello world!",
            "msgtype": "m.text",
        },
        "room_id": room_id,
        "event_id": event_id,
        "origin_server_ts": 152049794,
        "sender": sender,
        "type": "m.room.message",
    });

    let room_name = "The Maltese Falcon";
    let sender_display_name = "John Mastodon";
    let sender_avatar_url = "https://example.org/avatar.jpeg";

    let pos = Mutex::new(0);
    Mock::given(SlidingSyncMatcher)
        .respond_with(move |request: &Request| {
            let partial_request: PartialSlidingSyncRequest = request.body_json().unwrap();
            // Repeat the transaction id in the response, to validate sticky parameters.
            let mut pos = pos.lock().unwrap();
            *pos += 1;
            let pos_as_str = (*pos).to_string();
            ResponseTemplate::new(200).set_body_json(json!({
                "txn_id": partial_request.txn_id,
                "pos": pos_as_str,
                "rooms": {
                    "!a98sd12bjh:example.org": {
                        "name": room_name,
                        "initial": true,

                        "required_state": [
                            // Sender's member information.
                            {
                                "content": {
                                    "avatar_url": sender_avatar_url,
                                    "displayname": sender_display_name,
                                    "membership": "join"
                                },
                                "room_id": room_id,
                                "event_id": "$151800140517rfvjc:example.org",
                                "membership": "join",
                                "origin_server_ts": 151800140,
                                "sender": sender,
                                "state_key": sender,
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 2970366,
                                }
                            },

                            // Own member information.
                            {
                                "content": {
                                    "avatar_url": null,
                                    "displayname": "My Self",
                                    "membership": "join"
                                },
                                "room_id": room_id,
                                "event_id": "$151800140517rflkc:example.org",
                                "membership": "join",
                                "origin_server_ts": 151800140,
                                "sender": my_user_id.clone(),
                                "state_key": my_user_id,
                                "type": "m.room.member",
                                "unsigned": {
                                    "age": 2970366,
                                }
                            },

                            // Power levels.
                            json!({
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100,
                                        "m.room.message": 25
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100,
                                        sender: 0
                                    },
                                    "users_default": 0
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422
                                }
                            })
                        ],

                        "timeline": [
                            event_json.clone(),
                        ]
                    }
                },

                "extensions": {
                    "account_data": {
                        // Leave it empty for now.
                    }
                }
            }))
        })
        .mount(&server)
        .await;

    let notification_client = NotificationClient::builder(client).await.unwrap().build();
    let item =
        notification_client.get_notification_with_sliding_sync(room_id, event_id).await.unwrap();

    check_requests(
        server,
        &[json!({
            "conn_id": "notifications",
            "lists": {
                "invites": {
                    "ranges": [
                        [0, 16]
                    ],
                    "required_state": [
                        ["m.room.avatar", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.canonical_alias", ""],
                        ["m.room.name", ""],
                        ["m.room.power_levels", ""],
                    ],
                    "filters": {
                        "is_invite": true,
                        "is_tombstoned": false,
                        "not_room_types": ["m.space"],
                    },
                    "sort": ["by_recency", "by_name"],
                    "timeline_limit": 8,
                }
            },
            "room_subscriptions": {
                "!a98sd12bjh:example.org": {
                    "required_state": [
                        ["m.room.avatar", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.canonical_alias", ""],
                        ["m.room.name", ""],
                        ["m.room.power_levels", ""],
                    ],
                    "timeline_limit": 16,
                },
            },
            "extensions": {
                "account_data": {
                    "enabled": true,
                }
            }
        })],
    )
    .await;

    let NotificationStatus::Event(item) = item else {
        panic!("notification not found");
    };

    assert_matches::assert_matches!(item.event, NotificationEvent::Timeline(event) => {
        assert_eq!(event.event_type(), TimelineEventType::RoomMessage);
    });
    assert_eq!(item.sender_display_name.as_deref(), Some(sender_display_name));
    assert_eq!(item.sender_avatar_url.as_deref(), Some(sender_avatar_url));
    assert_eq!(item.room_display_name, room_name);
    assert_eq!(item.is_noisy, Some(false));
}
