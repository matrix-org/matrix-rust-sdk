use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use matrix_sdk::{
    ThreadingSupport,
    config::SyncSettings,
    test_utils::{
        logged_in_client_with_server,
        mocks::{MatrixMockServer, RoomContextResponseTemplate},
    },
};
use matrix_sdk_test::{
    JoinedRoomBuilder, SyncResponseBuilder, async_test, event_factory::EventFactory,
    mocks::mock_encryption_state,
};
use matrix_sdk_ui::{
    notification_client::{
        NotificationClient, NotificationEvent, NotificationItemsRequest, NotificationProcessSetup,
        NotificationStatus,
    },
    sync_service::SyncService,
};
use ruma::{
    RoomVersionId, event_id,
    events::{Mentions, TimelineEventType, room::member::MembershipState},
    mxc_uri, owned_user_id, room_id, user_id,
};
use serde_json::json;
use wiremock::{
    Mock, Request, ResponseTemplate,
    matchers::{header, method, path},
};

use crate::{
    mock_sync,
    sliding_sync::{PartialSlidingSyncRequest, SlidingSyncMatcher, check_requests},
};

#[async_test]
async fn test_notification_client_with_context() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let sender = user_id!("@user:example.org");
    let room_id = room_id!("!a98sd12bjh:example.org");
    let f = EventFactory::new().room(room_id).sender(sender);

    let content = "Hello world!";
    let event_id = event_id!("$example_event_id");
    let event = f.text_msg(content).event_id(event_id).server_ts(152049794).into_event();

    let sender_display_name = "John Mastodon";
    let sender_avatar_url = mxc_uri!("mxc://example.org/avatar");
    let sender_member_event = f
        .member(sender)
        .membership(MembershipState::Join)
        .display_name(sender_display_name)
        .avatar_url(sender_avatar_url)
        .into_raw_timeline();

    // First, mock a sync that contains a text message.
    server
        .sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(event.raw().clone()))
        .await;

    // Then, try to simulate receiving a notification for that message.
    let dummy_sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup =
        NotificationProcessSetup::SingleProcess { sync_service: dummy_sync_service };
    let notification_client = NotificationClient::new(client, process_setup).await.unwrap();

    // The notification client retrieves the event via `/rooms/*/context/`.
    server
        .mock_room_event_context()
        .ok(RoomContextResponseTemplate::new(event)
            .state_events(vec![sender_member_event.cast_unchecked()]))
        .mock_once()
        .mount()
        .await;

    // The encryption state is also fetched to figure whether the room is encrypted
    // or not.
    server.mock_room_state_encryption().plain().mount().await;

    let item = notification_client.get_notification_with_context(room_id, event_id).await.unwrap();

    assert_let!(NotificationStatus::Event(item) = item);

    assert_matches!(item.event, NotificationEvent::Timeline(event) => {
        assert_eq!(event.event_type(), TimelineEventType::RoomMessage);
    });
    assert_eq!(item.sender_display_name.as_deref(), Some(sender_display_name));
    assert_eq!(item.sender_avatar_url, Some(sender_avatar_url.to_string()));
}

#[async_test]
async fn test_subscribed_threads_get_notifications() {
    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
        })
        .build()
        .await;

    let sender = user_id!("@user:example.org");
    let room_id = room_id!("!a98sd12bjh:example.org");
    let f = EventFactory::new().room(room_id).sender(sender);

    // First, mock an empty sync so the room is known.
    server.mock_room_state_encryption().plain().mount().await;

    // To have access to the push rules context, we must know the own's member
    // event.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(
                f.member(client.user_id().unwrap())
                    .membership(MembershipState::Join)
                    .display_name("Jean-Michel Rouille"),
            ),
        )
        .await;

    // Sanity check: we can create push rules context.
    room.push_context().await.unwrap().unwrap();

    // Create a notification client.
    let sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup = NotificationProcessSetup::SingleProcess { sync_service };
    let notification_client = NotificationClient::new(client, process_setup).await.unwrap();

    // For a thread I'm subscribed to,
    let thread_root = event_id!("$thread_root");
    server
        .mock_room_get_thread_subscription()
        .match_thread_id(thread_root.to_owned())
        .ok(false)
        .expect(2)
        .mount()
        .await;

    let sender_member_event =
        f.member(sender).membership(MembershipState::Join).display_name("John Diaspora").into_raw();

    // Considering an in-thread message,
    let in_thread_event = {
        let event_id = event_id!("$example_event_id");
        let event = f
            .text_msg("hello to you too!")
            .event_id(event_id)
            .server_ts(152049794)
            .in_thread(thread_root, thread_root)
            .into_event();

        server
            .mock_room_event_context()
            .match_event_id()
            .ok(RoomContextResponseTemplate::new(event.clone())
                .state_events(vec![sender_member_event.clone()]))
            .mock_once()
            .mount()
            .await;

        // Then I get a notification for this message.
        let item =
            notification_client.get_notification_with_context(room_id, event_id).await.unwrap();
        assert_matches!(item, NotificationStatus::Event(..));

        event
    };

    // Considering the thread root event,
    let event = f
        .text_msg("hello world")
        .event_id(thread_root)
        .server_ts(152049793)
        .with_bundled_thread_summary(in_thread_event.raw().clone().cast_unchecked(), 1, false)
        .into_event();

    server
        .mock_room_event_context()
        .match_event_id()
        .ok(RoomContextResponseTemplate::new(event.clone()).state_events(vec![sender_member_event]))
        .mock_once()
        .mount()
        .await;

    // Then I get a notification for the thread root as well.
    let item =
        notification_client.get_notification_with_context(room_id, thread_root).await.unwrap();
    assert_matches!(item, NotificationStatus::Event(..));
}

#[async_test]
async fn test_unsubscribed_threads_get_notifications() {
    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
        })
        .build()
        .await;

    let sender = user_id!("@user:example.org");
    let room_id = room_id!("!a98sd12bjh:example.org");
    let f = EventFactory::new().room(room_id).sender(sender);

    // First, mock an empty sync so the room is known.
    server.mock_room_state_encryption().plain().mount().await;

    // To have access to the push rules context, we must know the own's member
    // event.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(
                f.member(client.user_id().unwrap())
                    .membership(MembershipState::Join)
                    .display_name("Jean-Michel Rouille"),
            ),
        )
        .await;

    // Sanity check: we can create push rules context.
    room.push_context().await.unwrap().unwrap();

    // Create a notification client.
    let sync_service = Arc::new(SyncService::builder(client.clone()).build().await.unwrap());
    let process_setup = NotificationProcessSetup::SingleProcess { sync_service };
    let notification_client = NotificationClient::new(client.clone(), process_setup).await.unwrap();

    // For a thread with an unknown subscription status (note: we're not mocking the
    // get endpoint, since 404 is equivalent to no thread status),
    let thread_root = event_id!("$thread_root");

    let sender_member_event =
        f.member(sender).membership(MembershipState::Join).display_name("John Diaspora").into_raw();

    // Considering a random in-thread message,
    let first_thread_response = event_id!("$thread_response1");
    let in_thread_event = {
        // Note: contains a mention, but not of me.
        let event = f
            .text_msg("hello to you too!")
            .event_id(first_thread_response)
            .server_ts(152049794)
            .in_thread(thread_root, thread_root)
            .mentions(Mentions::with_user_ids([owned_user_id!("@rando:example.org")]))
            .into_event();

        server
            .mock_room_event_context()
            .match_event_id()
            .ok(RoomContextResponseTemplate::new(event.clone())
                .state_events(vec![sender_member_event.clone()]))
            .mock_once()
            .mount()
            .await;

        // Then I don't get a notification for it.
        let item = notification_client
            .get_notification_with_context(room_id, first_thread_response)
            .await
            .unwrap();
        assert_matches!(item, NotificationStatus::EventFilteredOut);

        event
    };

    // Considering the thread root event,
    {
        let event = f
            .text_msg("hello world")
            .event_id(thread_root)
            .server_ts(152049793)
            .with_bundled_thread_summary(in_thread_event.raw().clone().cast_unchecked(), 1, false)
            .into_event();

        server
            .mock_room_event_context()
            .match_event_id()
            .ok(RoomContextResponseTemplate::new(event.clone())
                .state_events(vec![sender_member_event.clone()]))
            .mock_once()
            .mount()
            .await;

        // I do get a notification about it, because it's not technically in the thread.
        let item =
            notification_client.get_notification_with_context(room_id, thread_root).await.unwrap();
        assert_matches!(item, NotificationStatus::Event(..));
    }

    // But if a new in-thread event mentions me, then I would get subscribed,
    {
        let thread_response2 = event_id!("$thread_response2");
        // Note: event mentions me.
        let event = f
            .text_msg("hello world")
            .event_id(thread_response2)
            .server_ts(152049793)
            .in_thread(thread_root, first_thread_response)
            .mentions(Mentions::with_user_ids(vec![client.user_id().unwrap().to_owned()]))
            .into_event();

        server
            .mock_room_event_context()
            .match_event_id()
            .ok(RoomContextResponseTemplate::new(event.clone())
                .state_events(vec![sender_member_event]))
            .mock_once()
            .mount()
            .await;

        // Then I do get a notification for the thread root either.
        let item = notification_client
            .get_notification_with_context(room_id, thread_response2)
            .await
            .unwrap();
        assert_matches!(item, NotificationStatus::Event(..));
    }
}

#[async_test]
async fn test_notification_client_sliding_sync() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;

    let event_id = event_id!("$example_event_id");
    let event_id2 = event_id!("$example_event_id2");
    let sender = user_id!("@user:example.org");
    let sender_display_name = "John Mastodon";
    let room_avatar_url = mxc_uri!("mxc://example.org/room_avatar");
    let sender_avatar_url = mxc_uri!("mxc://example.org/avatar");
    let my_user_id = client.user_id().unwrap().to_owned();

    let event_factory = EventFactory::new().room(room_id);

    let room_create_event = event_factory.create(sender, RoomVersionId::V1).into_raw_sync();

    let sender_member_event = event_factory
        .member(sender)
        .display_name(sender_display_name)
        .avatar_url(sender_avatar_url)
        .membership(MembershipState::Join)
        .into_raw_sync();

    let own_member_event = event_factory
        .member(&my_user_id)
        .display_name("My self")
        .membership(MembershipState::Join)
        .into_raw_sync();

    let power_levels_event =
        event_factory.power_levels(&mut BTreeMap::new()).sender(sender).into_raw_sync();

    let room_avatar_event =
        event_factory.room_avatar().sender(sender).url(room_avatar_url).into_raw_sync();

    let event_json =
        event_factory.text_msg("Hello world!").event_id(event_id).sender(sender).into_raw_sync();

    let event_json2 = event_factory
        .text_msg("Hello world again!")
        .sender(sender)
        .event_id(event_id2)
        .into_raw_sync();

    let room_name = "The Maltese Falcon";
    let sender_display_name = "John Mastodon";

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
                            // The room creation event.
                            room_create_event,

                            // Sender's member information.
                            sender_member_event,

                            // Own member information.
                            own_member_event,

                            // Power levels.
                            power_levels_event,

                            // The room's avatar
                            room_avatar_event,
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
                        ["m.room.avatar", ""],
                        ["m.room.power_levels", ""],
                        ["m.room.join_rules", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                        ["m.room.create", ""],
                    ],
                    "filters": {
                        "is_invite": true,
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
                        ["m.room.avatar", ""],
                        ["m.room.power_levels", ""],
                        ["m.room.join_rules", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                        ["m.room.create", ""],
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
    assert_eq!(item.sender_avatar_url, Some(sender_avatar_url.to_string()));
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
    let sender_display_name = "John Mastodon";
    let sender_avatar_url = mxc_uri!("mxc://example.org/avatar");
    let my_user_id = client.user_id().unwrap().to_owned();

    let event_factory1 = EventFactory::new().room(room_id);
    let event_factory2 = EventFactory::new().room(room_id2);

    let sender_member_event_room1 = event_factory1
        .member(sender)
        .display_name(sender_display_name)
        .avatar_url(sender_avatar_url)
        .membership(MembershipState::Join)
        .into_raw_sync();

    let sender_member_event_room2 = event_factory2
        .member(sender)
        .display_name(sender_display_name)
        .membership(MembershipState::Join)
        .into_raw_sync();

    let own_member_event = event_factory1
        .member(&my_user_id)
        .display_name("My self")
        .membership(MembershipState::Join)
        .into_raw_sync();

    let invite_member_event = event_factory2
        .member(&my_user_id)
        .membership(MembershipState::Invite)
        .sender(sender)
        .event_id(invite_event_id)
        .into_raw_sync();

    let power_levels_event =
        event_factory1.power_levels(&mut BTreeMap::new()).sender(sender).into_raw_sync();

    let event_json =
        event_factory1.text_msg("Hello world!").event_id(event_id).sender(sender).into_raw_sync();

    let room_name = "The Maltese Falcon";
    let sender_display_name = "John Mastodon";

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
                            sender_member_event_room1,

                            // Own member information.
                            own_member_event,

                            // Power levels.
                            power_levels_event,
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
                            sender_member_event_room2,
                            // Invite event
                            invite_member_event,
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
    assert_eq!(item.sender_avatar_url, Some(sender_avatar_url.to_string()));
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
    let sender_display_name = "John Mastodon";
    let sender_avatar_url = mxc_uri!("mxc://example.org/avatar");
    let my_user_id = client.user_id().unwrap().to_owned();

    let event_factory1 = EventFactory::new().room(room_id);
    let event_factory2 = EventFactory::new().room(room_id2);

    let sender_member_event_room1 = event_factory1
        .member(sender)
        .display_name(sender_display_name)
        .avatar_url(sender_avatar_url)
        .membership(MembershipState::Join)
        .into_raw_sync();

    let sender_member_event_room2 = event_factory2
        .member(sender)
        .display_name(sender_display_name)
        .membership(MembershipState::Join)
        .into_raw_sync();

    let own_member_event = event_factory1
        .member(&my_user_id)
        .display_name("My self")
        .membership(MembershipState::Join)
        .into_raw_sync();

    let invite_member_event = event_factory2
        .member(&my_user_id)
        .membership(MembershipState::Invite)
        .sender(sender)
        .event_id(invite_event_id)
        .into_raw_sync();

    let power_levels_event =
        event_factory1.power_levels(&mut BTreeMap::new()).sender(sender).into_raw_sync();

    let event_json =
        event_factory1.text_msg("Hello world!").event_id(event_id).sender(sender).into_raw_sync();

    let room_name = "The Maltese Falcon";

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
                            sender_member_event_room1,

                            // Own member information.
                            own_member_event,

                            // Power levels.
                            power_levels_event,
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
                            sender_member_event_room2,
                            // Invite event
                            invite_member_event,
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
    assert_eq!(item.sender_avatar_url, Some(sender_avatar_url.to_string()));
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
    let sender_display_name = "John Mastodon";
    let sender_avatar_url = mxc_uri!("mxc://example.org/avatar");
    let my_user_id = client.user_id().unwrap().to_owned();

    let event_factory = EventFactory::new().room(room_id);

    let sender_member_event = event_factory
        .member(sender)
        .display_name(sender_display_name)
        .avatar_url(sender_avatar_url)
        .membership(MembershipState::Join)
        .into_raw_sync_state();

    let own_member_event = event_factory
        .member(&my_user_id)
        .display_name("My self")
        .membership(MembershipState::Join)
        .into_raw_sync_state();

    let power_levels_event =
        event_factory.power_levels(&mut BTreeMap::new()).sender(sender).into_raw_sync();

    let event_json =
        event_factory.text_msg("Hello world!").event_id(event_id).sender(sender).into_raw_sync();

    let event_json2 = event_factory
        .text_msg("Hello world again!")
        .sender(sender)
        .event_id(event_id2)
        .into_raw_sync();

    let room_name = "The Maltese Falcon";
    let sender_display_name = "John Mastodon";

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_state_bulk([sender_member_event.clone(), own_member_event.clone()]),
    );

    // First, mock a sync that contains a state event so we get a valid room for
    // room_id.
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    let pos = Mutex::new(0);
    Mock::given(SlidingSyncMatcher)
        .respond_with({
            let sender_member_event = sender_member_event.clone();
            move |request: &Request| {
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
                                sender_member_event,

                                // Own member information.
                                own_member_event,

                                // Power levels.
                                power_levels_event,
                            ],

                            "timeline": [
                                event_json,
                            ]
                        }
                    },

                    "extensions": {
                        "account_data": {
                            // Leave it empty for now.
                        }
                    }
                }))
            }
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
                    sender_member_event,
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

    assert_let!(NotificationStatus::Event(item) = item);
    assert_matches!(item.event, NotificationEvent::Timeline(event) => {
        assert_eq!(event.event_type(), TimelineEventType::RoomMessage);
    });
    assert_eq!(item.sender_display_name.as_deref(), Some(sender_display_name));
    assert_eq!(item.sender_avatar_url, Some(sender_avatar_url.to_string()));
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
    let event_id = event_id!("$example_event_id");

    let raw_event = EventFactory::new()
        .room(room_id)
        .sender(sender)
        .text_msg("Heya")
        .event_id(event_id)
        .into_raw_sync();

    let event_factory = EventFactory::new().room(room_id);

    let sender_member_event = event_factory
        .member(sender)
        .display_name(sender_display_name)
        .membership(MembershipState::Join)
        .into_raw_sync();

    let own_member_event = event_factory
        .member(&my_user_id)
        .display_name("My self")
        .membership(MembershipState::Join)
        .into_raw_sync();

    let power_levels_event =
        event_factory.sender(sender).power_levels(&mut BTreeMap::new()).into_raw_sync();

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
                            sender_member_event,

                            // Own member information.
                            own_member_event,

                            // Power levels.
                            power_levels_event,
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
    server
        .mock_room_event_context()
        .ok(RoomContextResponseTemplate::new(event).start("start").end("end"))
        .mock_once()
        .mount()
        .await;

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
    // was discarded as expected.
    let result =
        notification_client.get_notification_with_context(room_id, event_id).await.unwrap();

    assert_matches!(result, NotificationStatus::EventFilteredOut);
}
