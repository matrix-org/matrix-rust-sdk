use matrix_sdk::{sync::Notification, test_utils::mocks::MatrixMockServer};
use matrix_sdk_base::deserialized_responses::RawAnySyncOrStrippedTimelineEvent;
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, TestRoom, User, async_test, event_factory::EventFactory,
};
use ruma::{
    OwnedRoomId, event_id,
    events::{AnyStrippedStateEvent, StateEventType},
    serde::Raw,
};
use strass::assert_let;
use stream_assert::{assert_pending, assert_ready};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[async_test]
async fn test_notifications_joined() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = TestRoom::Default;
    let user_id = client.user_id().unwrap();
    let bob = User::Bob;
    let f = EventFactory::new().default_room();

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
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_state_bulk([
                f.power_levels()
                    .set_users([(user_id, 100), (bob.into(), 0)])
                    .sender(user_id)
                    .into_raw_sync_state(),
                f.member(user_id).display_name("example").into_raw_sync_state(),
            ]),
        )
        .await;

    assert_pending!(receiver_stream);

    // Sync with two notifications.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_bulk([
                f.text_msg("Hello example!")
                    .event_id(event_id!("$aaa"))
                    .server_ts(2189)
                    .sender(bob)
                    .into_raw_sync(),
                f.text_msg("How are you?")
                    .event_id(event_id!("$bbb"))
                    .server_ts(3189)
                    .sender(bob)
                    .into_raw_sync(),
            ]),
        )
        .await;

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
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = TestRoom::Invited;
    let user_id = client.user_id().unwrap();
    let bob = User::Bob;
    let f = EventFactory::new().room(room_id);

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

    let power_levels_event: Raw<AnyStrippedStateEvent> =
        f.power_levels().set_users([(user_id, 100), (bob.into(), 0)]).sender(user_id).into();

    server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id).add_state_bulk([
                power_levels_event,
                f.member(bob).into(),
                f.member(bob).invited(user_id).display_name("example").into(),
            ]),
        )
        .await;

    let (notif_room_id, notification) = assert_ready!(receiver_stream);
    assert_eq!(notif_room_id, room_id);
    assert_let!(RawAnySyncOrStrippedTimelineEvent::Stripped(raw_event) = notification.event);
    let event = raw_event.deserialize().unwrap();
    assert_eq!(event.event_type(), StateEventType::RoomMember);
    assert_eq!(event.state_key(), user_id);

    assert_pending!(receiver_stream);
}
