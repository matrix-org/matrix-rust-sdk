use assert_matches2::assert_matches;
use matrix_sdk::{config::SyncSettings, sync::Notification};
use matrix_sdk_base::deserialized_responses::RawAnySyncOrStrippedTimelineEvent;
use matrix_sdk_test::{
    async_test, stripped_state_event, sync_state_event, sync_timeline_event, test_json,
    InvitedRoomBuilder, JoinedRoomBuilder, SyncResponseBuilder,
};
use ruma::{
    events::{room::message::RoomMessageEventContent, Mentions, StateEventType},
    room_id,
    serde::Raw,
    OwnedRoomId,
};
use stream_assert::{assert_pending, assert_ready};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::{logged_in_client_with_server, mock_sync};

#[async_test]
async fn test_notifications_joined() {
    let (client, server) = logged_in_client_with_server().await;
    let room_id = room_id!("!joined_room:localhost");
    let user_id = client.user_id().unwrap();

    let (sender, receiver) = mpsc::channel::<(OwnedRoomId, Notification)>(10);
    let mut receiver_stream = ReceiverStream::new(receiver);

    client
        .register_notification_handler(move |notification, room, _client| {
            let sender = sender.clone();
            async move {
                sender.send((room.room_id().to_owned(), notification)).await.unwrap();
            }
        })
        .await;

    // Set up the room state, no notifications.
    let mut sync_builder = SyncResponseBuilder::new();
    let joined_room = JoinedRoomBuilder::new(room_id).add_state_bulk([
        Raw::new(&*test_json::POWER_LEVELS).unwrap().cast(),
        sync_state_event!({
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "join"
            },
            "event_id": "$join_example",
            "origin_server_ts": 151800140,
            "sender": user_id,
            "state_key": user_id,
            "type": "m.room.member",
        }),
    ]);
    sync_builder.add_joined_room(joined_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();
    server.reset().await;

    assert_pending!(receiver_stream);

    // Sync with two notifications.
    let first_message = RoomMessageEventContent::text_plain("Hello example!")
        .add_mentions(Mentions::with_user_ids([client.user_id().unwrap().to_owned()]));
    let second_message = RoomMessageEventContent::text_plain("How are you?");

    let joined_room = JoinedRoomBuilder::new(room_id).add_timeline_bulk([
        sync_timeline_event!({
            "content": first_message,
            "type": "m.room.message",
            "event_id": "$aaa",
            "origin_server_ts": 2189,
            "sender": "@bob:example.com",
        }),
        sync_timeline_event!({
            "content": second_message,
            "type": "m.room.message",
            "event_id": "$bbb",
            "origin_server_ts": 3189,
            "sender": "@bob:example.com",
        }),
    ]);
    sync_builder.add_joined_room(joined_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();

    let (notif_room_id, notification) = assert_ready!(receiver_stream);
    assert_eq!(notif_room_id, room_id);
    assert_matches!(notification.event, RawAnySyncOrStrippedTimelineEvent::Sync(raw_event));
    let event = raw_event.deserialize().unwrap();
    assert_eq!(event.event_id(), "$aaa");

    let (notif_room_id, notification) = assert_ready!(receiver_stream);
    assert_eq!(notif_room_id, room_id);
    assert_matches!(notification.event, RawAnySyncOrStrippedTimelineEvent::Sync(raw_event));
    let event = raw_event.deserialize().unwrap();
    assert_eq!(event.event_id(), "$bbb");

    assert_pending!(receiver_stream);
}

#[async_test]
async fn test_notifications_invite() {
    let (client, server) = logged_in_client_with_server().await;
    let room_id = room_id!("!invited_room:localhost");
    let user_id = client.user_id().unwrap();

    let (sender, receiver) = mpsc::channel::<(OwnedRoomId, Notification)>(10);
    let mut receiver_stream = ReceiverStream::new(receiver);

    client
        .register_notification_handler(move |notification, room, _client| {
            let sender = sender.clone();
            async move {
                sender.send((room.room_id().to_owned(), notification)).await.unwrap();
            }
        })
        .await;

    let mut sync_builder = SyncResponseBuilder::new();
    let invited_room = InvitedRoomBuilder::new(room_id).add_state_bulk([
        Raw::new(&*test_json::POWER_LEVELS).unwrap().cast(),
        stripped_state_event!({
            "content": {
                "membership": "join"
            },
            "sender": "@bob:localhost",
            "state_key": "@bob:localhost",
            "type": "m.room.member",
        }),
        stripped_state_event!({
            "content": {
                "avatar_url": null,
                "displayname": "example",
                "membership": "invite"
            },
            "sender": "@bob:localhost",
            "state_key": user_id,
            "type": "m.room.member",
        }),
    ]);
    sync_builder.add_invited_room(invited_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();

    let (notif_room_id, notification) = assert_ready!(receiver_stream);
    assert_eq!(notif_room_id, room_id);
    assert_matches!(notification.event, RawAnySyncOrStrippedTimelineEvent::Stripped(raw_event));
    let event = raw_event.deserialize().unwrap();
    assert_eq!(event.event_type(), StateEventType::RoomMember);
    assert_eq!(event.state_key(), user_id);

    assert_pending!(receiver_stream);
}
