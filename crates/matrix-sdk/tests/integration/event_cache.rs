use std::time::Duration;

use assert_matches2::{assert_let, assert_matches};
use matrix_sdk::{
    event_cache::{EventCacheError, RoomEventCacheUpdate},
    test_utils::{logged_in_client, logged_in_client_with_server},
};
use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use matrix_sdk_test::{
    async_test, sync_timeline_event, EventBuilder, JoinedRoomBuilder, SyncResponseBuilder,
};
use ruma::{
    events::{
        room::message::{MessageType, RoomMessageEventContent},
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
    },
    room_id, user_id,
};
use tokio::time::timeout;

use crate::mock_sync;

fn assert_event_matches_msg(event: &SyncTimelineEvent, expected: &str) {
    let event = event.event.deserialize().unwrap();
    assert_let!(
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(message)) = event
    );
    let message = message.as_original().unwrap();
    assert_let!(MessageType::Text(text) = &message.content.msgtype);
    assert_eq!(text.body, expected);
}

#[async_test]
async fn test_must_explicitly_subscribe() {
    let client = logged_in_client(None).await;

    let event_cache = client.event_cache();

    // If I create a room event subscriber for a room before subscribing the event
    // cache,
    let room_id = room_id!("!omelette:fromage.fr");
    let result = event_cache.for_room(room_id).await;

    // Then it fails, because one must explicitly call `.subscribe()` on the event
    // cache.
    assert_matches!(result, Err(EventCacheError::NotSubscribedYet));
}

#[async_test]
async fn test_cant_subscribe_to_unknown_room() {
    let client = logged_in_client(None).await;

    let event_cache = client.event_cache();

    // First, subscribe to the event cache.
    event_cache.subscribe().unwrap();

    // If I create a room event subscriber for a room unknown to the client,
    let room_id = room_id!("!omelette:fromage.fr");
    let result = event_cache.for_room(room_id).await;

    // Then it fails, because this particular room is still unknown to the client.
    assert_let!(Err(EventCacheError::RoomNotFound(observed_room_id)) = result);
    assert_eq!(observed_room_id, room_id);
}

#[async_test]
async fn test_add_initial_events() {
    let (client, server) = logged_in_client_with_server().await;

    let event_cache = client.event_cache();

    // If I sync and get informed I've joined The Room, but with no events,
    let room_id = room_id!("!omelette:fromage.fr");

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    let response_body = sync_builder.build_json_sync_response();

    mock_sync(&server, response_body, None).await;
    client.sync_once(Default::default()).await.unwrap();
    server.reset().await;

    // If I create a room event subscriber, just after subscribing,
    event_cache.subscribe().unwrap();

    let (room_event_cache, _drop_handles) = event_cache.for_room(room_id).await.unwrap();
    let (events, mut subscriber) = room_event_cache.subscribe().await.unwrap();

    // Then at first it's empty, and the subscriber doesn't yield anything.
    assert!(events.is_empty());
    assert!(subscriber.is_empty());

    // And after a sync, yielding updates to two rooms,
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        EventBuilder::new().make_sync_message_event(
            user_id!("@dexter:lab.org"),
            RoomMessageEventContent::text_plain("bonjour monde"),
        ),
    ));

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id!("!parallel:universe.uk")).add_timeline_event(
            EventBuilder::new().make_sync_message_event(
                user_id!("@dexter:lab.org"),
                RoomMessageEventContent::text_plain("hi i'm learning French"),
            ),
        ),
    );

    let response_body = sync_builder.build_json_sync_response();

    mock_sync(&server, response_body, None).await;
    client.sync_once(Default::default()).await.unwrap();
    server.reset().await;

    // It does receive one update,
    let update = timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("timeout after receiving a sync update")
        .expect("should've received a room event cache update");

    // Which contains the event that was sent beforehand.
    assert_let!(RoomEventCacheUpdate::Append { events, .. } = update);
    assert_eq!(events.len(), 1);
    assert_event_matches_msg(&events[0], "bonjour monde");

    // And when I later add initial events to this room,

    // XXX: when we get rid of `add_initial_events`, we can keep this test as a
    // smoke test for the event cache.
    event_cache
        .add_initial_events(
            room_id,
            vec![SyncTimelineEvent::new(sync_timeline_event!({
                "sender": "@dexter:lab.org",
                "type": "m.room.message",
                "event_id": "$ida",
                "origin_server_ts": 12344446,
                "content": { "body":"new choice!", "msgtype": "m.text" },
            }))],
        )
        .await
        .unwrap();

    // Then I receive an update that the room has been cleared,
    let update = timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("timeout after receiving a sync update")
        .expect("should've received a room event cache update");
    assert_let!(RoomEventCacheUpdate::Clear = update);

    // Before receiving the "initial" event.
    let update = timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("timeout after receiving a sync update")
        .expect("should've received a room event cache update");
    assert_let!(RoomEventCacheUpdate::Append { events, .. } = update);
    assert_eq!(events.len(), 1);
    assert_event_matches_msg(&events[0], "new choice!");

    // That's all, folks!
    assert!(subscriber.is_empty());
}
