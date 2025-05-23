use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use assert_matches::assert_matches;
use matrix_sdk::{
    config::SyncSettings,
    test_utils::{logged_in_client_with_server, mocks::MatrixMockServer},
};
use matrix_sdk_test::{
    async_test, mocks::mock_encryption_state, JoinedRoomBuilder, StateTestEvent,
    SyncResponseBuilder,
};
use matrix_sdk_ui::{
    notification_client::{
        NotificationClient, NotificationEvent, NotificationItemsRequest, NotificationProcessSetup,
        NotificationStatus,
    },
    sync_service::SyncService,
};
use ruma::{event_id, events::TimelineEventType, room_id, user_id};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path},
    Mock, Request, ResponseTemplate,
};

use crate::{
    mock_sync,
    sliding_sync::{check_requests, PartialSlidingSyncRequest, SlidingSyncMatcher},
};

#[async_test]
async fn test_notification_client_with_context() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let content = "Hello world!";
    let event_id = event_id!("$example_event_id");
    let server_ts = 152049794;
    let sender = user_id!("@user:example.org");
    let event_json = json!({
        "content": {
            "body": content,
            "msgtype": "m.text",
        },
        "room_id": room_id,
        "event_id": event_id,
        "origin_server_ts": server_ts,
        "sender": sender,
        "type": "m.room.message",
    });

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(
            EventFactory::new()
                .text_msg(content)
                .event_id(event_id)
                .server_ts(server_ts)
                .sender(sender)
                .into_raw_sync(),
        ),
    );

    // First, mock a sync that contains a text message.
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Then, try to simulate receiving a notification for that message.
    let dummy_sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup =
        NotificationProcessSetup::SingleProcess { sync_service: dummy_sync_service };
    let notification_client = NotificationClient::new(client, process_setup).await.unwrap();

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

    assert_matches!(item.event, NotificationEvent::Timeline(event) => {
        assert_eq!(event.event_type(), TimelineEventType::RoomMessage);
    });
    assert_eq!(item.sender_display_name.as_deref(), Some("John Mastodon"));
    assert_eq!(item.sender_avatar_url.as_deref(), Some("https://example.org/avatar.jpeg"));
}

#[async_test]
async fn test_notification_client_sliding_sync() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;

    let event_id = event_id!("$example_event_id");
    let event_id2 = event_id!("$example_event_id2");
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
    let event_json2 = json!({
        "content": {
            "body": "Hello world again!",
            "msgtype": "m.text",
        },
        "room_id": room_id,
        "event_id": event_id2,
        "origin_server_ts": 152049795,
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
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100,
                                        "m.room.message": 25,
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100,
                                        sender: 0,
                                    },
                                    "users_default": 0,
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422,
                                },
                            },
                        ],

                        "timeline": [
                            event_json.clone(),
                            event_json2.clone(),
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

    let dummy_sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup =
        NotificationProcessSetup::SingleProcess { sync_service: dummy_sync_service };
    let notification_client = NotificationClient::new(client, process_setup).await.unwrap();
    let mut result = notification_client
        .get_notifications_with_sliding_sync(&[NotificationItemsRequest {
            room_id: room_id.to_owned(),
            event_ids: vec![event_id.to_owned(), event_id2.to_owned()],
        }])
        .await
        .unwrap();

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
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.canonical_alias", ""],
                        ["m.room.name", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                    ],
                    "filters": {
                        "is_invite": true,
                        "not_room_types": ["m.space"],
                    },
                    "timeline_limit": 8,
                }
            },
            "room_subscriptions": {
                "!a98sd12bjh:example.org": {
                    "required_state": [
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.canonical_alias", ""],
                        ["m.room.name", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
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

    let Some(Ok(item)) = result.remove(event_id) else {
        panic!("fetching notification for {event_id} failed");
    };
    let NotificationStatus::Event(item) = item else {
        panic!("notification for {event_id} not found");
    };

    let Some(Ok(item2)) = result.remove(event_id2) else {
        panic!("fetching notification for {event_id2} failed");
    };
    let NotificationStatus::Event(_) = item2 else {
        panic!("notification for {event_id2} not found");
    };

    assert_matches!(item.event, NotificationEvent::Timeline(event) => {
        assert_eq!(event.event_type(), TimelineEventType::RoomMessage);
    });
    assert_eq!(item.sender_display_name.as_deref(), Some(sender_display_name));
    assert_eq!(item.sender_avatar_url.as_deref(), Some(sender_avatar_url));
    assert_eq!(item.room_computed_display_name, sender_display_name);
    assert_eq!(item.is_noisy, Some(false));
}

#[async_test]
async fn test_notification_client_sliding_sync_invites() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let room_id2 = room_id!("!a98sd12bjh2:example.org");
    let (client, server) = logged_in_client_with_server().await;

    let event_id = event_id!("$example_event_id");
    let invite_event_id = event_id!("$invite_event_id");
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
    let invite_event_json = json!({
        "content": {
            "displayname": "Alice Margatroid",
            "membership": "invite"
        },
        "room_id": room_id,
        // No event id, as it's an invite and it's a stripped event which lacks this info.
        "origin_server_ts": 152049794,
        "sender": sender,
        "state_key": client.user_id().unwrap().to_owned(),
        "type": "m.room.member",
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
                    room_id: {
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
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100,
                                        "m.room.message": 25,
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100,
                                        sender: 0,
                                    },
                                    "users_default": 0,
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422,
                                },
                            },
                        ],

                        "timeline": [
                            event_json.clone(),
                        ]
                    },
                    room_id2: {
                        "name": "invite room",
                        "initial": true,
                        "invite_state": [
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
                            // Invite event
                            invite_event_json,
                        ],
                        "timeline": [],
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

    let dummy_sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup =
        NotificationProcessSetup::SingleProcess { sync_service: dummy_sync_service };
    let notification_client = NotificationClient::new(client, process_setup).await.unwrap();
    let mut result = notification_client
        .get_notifications_with_sliding_sync(&[
            NotificationItemsRequest {
                room_id: room_id.to_owned(),
                event_ids: vec![event_id.to_owned()],
            },
            NotificationItemsRequest {
                room_id: room_id2.to_owned(),
                event_ids: vec![invite_event_id.to_owned()],
            },
        ])
        .await
        .unwrap();

    let Some(Ok(item)) = result.remove(event_id) else {
        panic!("fetching notification for {event_id} failed");
    };
    let NotificationStatus::Event(item) = item else {
        panic!("notification for {event_id} not found");
    };

    let Some(Ok(invite)) = result.remove(invite_event_id) else {
        panic!("fetching notification for {invite_event_id} failed");
    };
    let NotificationStatus::Event(_) = invite else {
        panic!("notification for {invite_event_id} not found");
    };

    assert_matches!(item.event, NotificationEvent::Timeline(event) => {
        assert_eq!(event.event_type(), TimelineEventType::RoomMessage);
    });
    assert_eq!(item.sender_display_name.as_deref(), Some(sender_display_name));
    assert_eq!(item.sender_avatar_url.as_deref(), Some(sender_avatar_url));
    assert_eq!(item.room_computed_display_name, sender_display_name);
    assert_eq!(item.is_noisy, Some(false));
}

#[async_test]
async fn test_notification_client_sliding_sync_invites_with_event_id() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let room_id2 = room_id!("!a98sd12bjh2:example.org");
    let (client, server) = logged_in_client_with_server().await;

    let event_id = event_id!("$example_event_id");
    let invite_event_id = event_id!("$invite_event_id");
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
    let invite_event_json = json!({
        "content": {
            "displayname": "Alice Margatroid",
            "membership": "invite"
        },
        "room_id": room_id,
        // This should never exist in a well-written server, but we need to test it.
        "event_id": invite_event_id,
        "origin_server_ts": 152049794,
        "sender": sender,
        "state_key": client.user_id().unwrap().to_owned(),
        "type": "m.room.member",
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
                    room_id: {
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
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100,
                                        "m.room.message": 25,
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100,
                                        sender: 0,
                                    },
                                    "users_default": 0,
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422,
                                },
                            },
                        ],

                        "timeline": [
                            event_json.clone(),
                        ]
                    },
                    room_id2: {
                        "name": "invite room",
                        "initial": true,
                        "invite_state": [
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
                            // Invite event
                            invite_event_json,
                        ],
                        "timeline": [],
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

    let dummy_sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup =
        NotificationProcessSetup::SingleProcess { sync_service: dummy_sync_service };
    let notification_client = NotificationClient::new(client, process_setup).await.unwrap();
    let mut result = notification_client
        .get_notifications_with_sliding_sync(&[
            NotificationItemsRequest {
                room_id: room_id.to_owned(),
                event_ids: vec![event_id.to_owned()],
            },
            NotificationItemsRequest {
                room_id: room_id2.to_owned(),
                event_ids: vec![invite_event_id.to_owned()],
            },
        ])
        .await
        .unwrap();

    let Some(Ok(item)) = result.remove(event_id) else {
        panic!("fetching notification for {event_id} failed");
    };
    let NotificationStatus::Event(item) = item else {
        panic!("notification for {event_id} not found");
    };

    let Some(Ok(invite)) = result.remove(invite_event_id) else {
        panic!("fetching notification for {invite_event_id} failed");
    };
    let NotificationStatus::Event(_) = invite else {
        panic!("notification for {invite_event_id} not found");
    };

    assert_matches!(item.event, NotificationEvent::Timeline(event) => {
        assert_eq!(event.event_type(), TimelineEventType::RoomMessage);
    });
    assert_eq!(item.sender_display_name.as_deref(), Some(sender_display_name));
    assert_eq!(item.sender_avatar_url.as_deref(), Some(sender_avatar_url));
    assert_eq!(item.room_computed_display_name, sender_display_name);
    assert_eq!(item.is_noisy, Some(false));
}

#[async_test]
async fn test_notification_client_mixed() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;

    let event_id = event_id!("$example_event_id");
    let event_id2 = event_id!("$example_event_id2");
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
    let event_json2 = json!({
        "content": {
            "body": "Hello world again!",
            "msgtype": "m.text",
        },
        "room_id": room_id,
        "event_id": event_id2,
        "origin_server_ts": 152049795,
        "sender": sender,
        "type": "m.room.message",
    });

    let room_name = "The Maltese Falcon";
    let sender_display_name = "John Mastodon";
    let sender_avatar_url = "https://example.org/avatar.jpeg";

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_state_event(StateTestEvent::Custom(json!(
                {
                    "content": {
                        "avatar_url": "https://example.org/avatar.jpeg",
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
                }
            )))
            .add_state_event(StateTestEvent::Custom(json!(
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
                }
            ))),
    );

    // First, mock a sync that contains a state event so we get a valid room for
    // room_id.
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

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
                    room_id: {
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
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100,
                                        "m.room.message": 25,
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100,
                                        sender: 0,
                                    },
                                    "users_default": 0,
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422,
                                },
                            },
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

    {
        // The notification client retrieves the event via `/rooms/*/context/`.
        Mock::given(method("GET"))
            .and(path(format!("/_matrix/client/r0/rooms/{room_id}/context/{event_id2}")))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event": event_json2,
                "state": [
                    {
                        "content": {
                            "avatar_url": "https://example.org/avatar.jpeg",
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
                    }
                ],
            })))
            .mount(&server)
            .await;

        // The encryption state is also fetched to figure whether the room is encrypted
        // or not.
        mock_encryption_state(&server, false).await;
    }

    let dummy_sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup =
        NotificationProcessSetup::SingleProcess { sync_service: dummy_sync_service };
    let notification_client = NotificationClient::new(client, process_setup).await.unwrap();
    let mut result = notification_client
        .get_notifications(&[NotificationItemsRequest {
            room_id: room_id.to_owned(),
            event_ids: vec![event_id.to_owned(), event_id2.to_owned()],
        }])
        .await
        .unwrap();

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "conn_id": "notifications",
            "lists": {
                "invites": {
                    "ranges": [
                        [0, 16]
                    ],
                    "required_state": [
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.canonical_alias", ""],
                        ["m.room.name", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                    ],
                    "filters": {
                        "is_invite": true,
                        "not_room_types": ["m.space"],
                    },
                    "timeline_limit": 8,
                }
            },
            "room_subscriptions": {
                room_id: {
                    "required_state": [
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.canonical_alias", ""],
                        ["m.room.name", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                    ],
                    "timeline_limit": 16,
                },
            },
            "extensions": {
                "account_data": {
                    "enabled": true,
                }
            }
        })))
        .mount(&server)
        .await;

    let Some(Ok(item)) = result.remove(event_id) else {
        panic!("fetching notification from sliding sync failed");
    };

    let _ = result.remove(event_id2).expect("fetching notification from /context failed");

    assert_matches!(item.event, NotificationEvent::Timeline(event) => {
        assert_eq!(event.event_type(), TimelineEventType::RoomMessage);
    });
    assert_eq!(item.sender_display_name.as_deref(), Some(sender_display_name));
    assert_eq!(item.sender_avatar_url.as_deref(), Some(sender_avatar_url));
    assert_eq!(item.room_computed_display_name, sender_display_name);
    assert_eq!(item.is_noisy, Some(false));
}

#[async_test]
async fn test_notification_client_sliding_sync_filters_out_events_from_ignored_users() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let sender = user_id!("@user:example.org");
    let my_user_id = client.user_id().unwrap().to_owned();

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room_name = "The Maltese Falcon";
    let sender_display_name = "John Mastodon";
    let sender_avatar_url = "https://example.org/avatar.jpeg";
    let event_id = event_id!("$example_event_id");

    let raw_event = EventFactory::new()
        .room(room_id)
        .sender(sender)
        .text_msg("Heya")
        .event_id(event_id)
        .into_raw_sync();

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
                    room_id: {
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
                            {
                                "content": {
                                    "ban": 50,
                                    "events": {
                                        "m.room.avatar": 50,
                                        "m.room.canonical_alias": 50,
                                        "m.room.history_visibility": 100,
                                        "m.room.name": 50,
                                        "m.room.power_levels": 100,
                                        "m.room.message": 25,
                                    },
                                    "events_default": 0,
                                    "invite": 0,
                                    "kick": 50,
                                    "redact": 50,
                                    "state_default": 50,
                                    "users": {
                                        "@example:localhost": 100,
                                        sender: 0,
                                    },
                                    "users_default": 0,
                                },
                                "event_id": "$15139375512JaHAW:localhost",
                                "origin_server_ts": 151393755,
                                "sender": "@example:localhost",
                                "state_key": "",
                                "type": "m.room.power_levels",
                                "unsigned": {
                                    "age": 703422,
                                },
                            },
                        ],

                        "timeline": [
                            raw_event,
                        ]
                    }
                },

                "extensions": {
                    "account_data": {
                        "global": [{
                            "type": "m.ignored_user_list",
                            "content": {
                                "ignored_users": { sender: {} }
                            }
                        }]
                    }
                }
            }))
        })
        .mount(server.server())
        .await;

    let dummy_sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup =
        NotificationProcessSetup::SingleProcess { sync_service: dummy_sync_service };
    let notification_client = NotificationClient::new(client, process_setup).await.unwrap();
    let mut result = notification_client
        .get_notifications_with_sliding_sync(&[NotificationItemsRequest {
            room_id: room_id.to_owned(),
            event_ids: vec![event_id.to_owned()],
        }])
        .await
        .unwrap();

    let Some(Ok(item)) = result.remove(event_id) else {
        panic!("fetching notification for {event_id} failed");
    };
    let NotificationStatus::EventFilteredOut = item else {
        panic!("notification for {event_id} was not filtered out");
    };
}

#[async_test]
async fn test_notification_client_context_filters_out_events_from_ignored_users() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let sender = user_id!("@user:example.org");
    let room_id = room_id!("!a98sd12bjh:example.org");
    let event_id = event_id!("$example_event_id");

    server.sync_joined_room(&client, room_id).await;

    // Add mock for sliding sync so we get the ignored user list from its account
    // data
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
                "rooms": {},

                "extensions": {
                    "account_data": {
                        "global": [{
                            "type": "m.ignored_user_list",
                            "content": {
                                "ignored_users": { sender: {} }
                            }
                        }]
                    }
                }
            }))
        })
        .mount(server.server())
        .await;

    let event = EventFactory::new()
        .room(room_id)
        .sender(sender)
        .text_msg("Heya")
        .event_id(event_id)
        .into_event();

    // Mock the /context response
    server.mock_room_event_context().ok(event, "start", "end").mock_once().mount().await;

    let dummy_sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup =
        NotificationProcessSetup::SingleProcess { sync_service: dummy_sync_service };
    let notification_client = NotificationClient::new(client, process_setup).await.unwrap();

    // Call sync first so we get the list of ignored users in the notification
    // client This should still work in a real life usage
    let _ = notification_client
        .get_notifications_with_sliding_sync(&[NotificationItemsRequest {
            room_id: room_id.to_owned(),
            event_ids: vec![event_id.to_owned()],
        }])
        .await;

    // If the event is not found even though there was a mocked response for it, it
    // was discarded as expected
    let result =
        notification_client.get_notification_with_context(room_id, event_id).await.unwrap();

    assert!(result.is_none());
}
