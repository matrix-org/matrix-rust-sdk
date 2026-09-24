use std::collections::BTreeMap;

use matrix_sdk::{
    config::{SyncSettings, SyncToken},
    sync::Notification,
};
use matrix_sdk_base::deserialized_responses::RawAnySyncOrStrippedTimelineEvent;
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, SyncResponseBuilder, async_test,
    event_factory::EventFactory,
};
use ruma::{
    Int, OwnedRoomId, event_id,
    events::{AnyStrippedStateEvent, StateEventType},
    owned_user_id, room_id,
    serde::Raw,
    user_id,
};
use strass::assert_let;
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
    let f = EventFactory::new().room(room_id).sender(user_id!("@example:localhost"));
    let mut users = BTreeMap::new();
    users.insert(owned_user_id!("@example:localhost"), Int::new(100).unwrap());
    users.insert(owned_user_id!("@bob:localhost"), Int::new(0).unwrap());
    let joined_room = JoinedRoomBuilder::new(room_id).add_state_bulk([
        f.power_levels(&mut users).into_raw_sync_state(),
        f.member(user_id).display_name("example").into_raw_sync_state(),
    ]);
    sync_builder.add_joined_room(joined_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default().token(SyncToken::NoToken)).await.unwrap();
    server.reset().await;

    assert_pending!(receiver_stream);

    // Sync with two notifications.
    let f = EventFactory::new();
    let joined_room = JoinedRoomBuilder::new(room_id).add_timeline_bulk([
        f.text_msg("Hello example!")
            .event_id(event_id!("$aaa"))
            .server_ts(2189)
            .sender(user_id!("@bob:example.com"))
            .into_raw_sync(),
        f.text_msg("How are you?")
            .event_id(event_id!("$bbb"))
            .server_ts(3189)
            .sender(user_id!("@bob:example.com"))
            .into_raw_sync(),
    ]);
    sync_builder.add_joined_room(joined_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default().token(SyncToken::NoToken)).await.unwrap();

    let (notif_room_id, notification) = assert_ready!(receiver_stream);
    assert_eq!(notif_room_id, room_id);
    assert_let!(RawAnySyncOrStrippedTimelineEvent::Sync(raw_event) = notification.event);
    let event = raw_event.deserialize().unwrap();
    assert_eq!(event.event_id(), "$aaa");

    let (notif_room_id, notification) = assert_ready!(receiver_stream);
    assert_eq!(notif_room_id, room_id);
    assert_let!(RawAnySyncOrStrippedTimelineEvent::Sync(raw_event) = notification.event);
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
    let f = EventFactory::new().room(room_id).sender(user_id!("@example:localhost"));
    let mut users = BTreeMap::new();
    users.insert(owned_user_id!("@example:localhost"), Int::new(100).unwrap());
    users.insert(owned_user_id!("@bob:localhost"), Int::new(0).unwrap());
    let power_levels_event: Raw<AnyStrippedStateEvent> = f.power_levels(&mut users).into();
    // Factory sends @example by default, but in reality these events come from Bob.
    let bob = user_id!("@bob:localhost");
    let invited_room = InvitedRoomBuilder::new(room_id).add_state_bulk([
        power_levels_event,
        f.member(bob).sender(bob).into(),
        f.member(bob).sender(bob).invited(user_id).display_name("example").into(),
    ]);
    sync_builder.add_invited_room(invited_room);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(SyncSettings::default()).await.unwrap();

    let (notif_room_id, notification) = assert_ready!(receiver_stream);
    assert_eq!(notif_room_id, room_id);
    assert_let!(RawAnySyncOrStrippedTimelineEvent::Stripped(raw_event) = notification.event);
    let event = raw_event.deserialize().unwrap();
    assert_eq!(event.event_type(), StateEventType::RoomMember);
    assert_eq!(event.state_key(), user_id);

    assert_pending!(receiver_stream);
}
