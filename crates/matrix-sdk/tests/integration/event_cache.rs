use std::time::Duration;

use assert_matches2::{assert_let, assert_matches};
use matrix_sdk::{
    event_cache::{BackPaginationOutcome, EventCacheError, RoomEventCacheUpdate},
    test_utils::logged_in_client_with_server,
};
use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
use matrix_sdk_test::{
    async_test, sync_timeline_event, EventBuilder, JoinedRoomBuilder, SyncResponseBuilder,
};
use ruma::{
    event_id,
    events::{
        room::message::{MessageType, RoomMessageEventContent},
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, AnyTimelineEvent,
    },
    room_id,
    serde::Raw,
    user_id,
};
use serde_json::json;
use tokio::{spawn, time::timeout};
use wiremock::{
    matchers::{header, method, path_regex, query_param},
    Mock, MockServer, ResponseTemplate,
};

use crate::mock_sync;

#[track_caller]
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
    let (client, server) = logged_in_client_with_server().await;

    let room_id = room_id!("!omelette:fromage.fr");

    // Make sure the client is aware of the room.
    {
        let mut sync_builder = SyncResponseBuilder::new();
        sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        let response_body = sync_builder.build_json_sync_response();

        mock_sync(&server, response_body, None).await;
        client.sync_once(Default::default()).await.unwrap();
        server.reset().await;
    }

    // If I create a room event subscriber for a room before subscribing the event
    // cache,
    let room = client.get_room(room_id).unwrap();
    let result = room.event_cache().await;

    // Then it fails, because one must explicitly call `.subscribe()` on the event
    // cache.
    assert_matches!(result, Err(EventCacheError::NotSubscribedYet));
}

#[async_test]
async fn test_add_initial_events() {
    let (client, server) = logged_in_client_with_server().await;

    // Immediately subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    // If I sync and get informed I've joined The Room, but with no events,
    let room_id = room_id!("!omelette:fromage.fr");

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    let response_body = sync_builder.build_json_sync_response();

    mock_sync(&server, response_body, None).await;
    client.sync_once(Default::default()).await.unwrap();
    server.reset().await;

    // If I create a room event subscriber,

    let room = client.get_room(room_id).unwrap();
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
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
    client
        .event_cache()
        .add_initial_events(
            room_id,
            vec![SyncTimelineEvent::new(sync_timeline_event!({
                "sender": "@dexter:lab.org",
                "type": "m.room.message",
                "event_id": "$ida",
                "origin_server_ts": 12344446,
                "content": { "body":"new choice!", "msgtype": "m.text" },
            }))],
            None,
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

macro_rules! non_sync_events {
    ( @_ $builder:expr, [ ( $room_id:expr , $event_id:literal : $msg:literal ) $(, $( $rest:tt )* )? ] [ $( $accumulator:tt )* ] ) => {
        non_sync_events!(
            @_ $builder,
            [ $( $( $rest )* )? ]
            [ $( $accumulator )*
              $builder.make_message_event_with_id(
                user_id!("@a:b.c"),
                $room_id,
                event_id!($event_id),
                RoomMessageEventContent::text_plain($msg)
              ),
            ]
        )
    };

    ( @_ $builder:expr, [] [ $( $accumulator:tt )* ] ) => {
        vec![ $( $accumulator )* ]
    };

    ( $builder:expr, [ $( $all:tt )* ] ) => {
        non_sync_events!( @_ $builder, [ $( $all )* ] [] )
    };
}

/// Puts a mounting point for /messages for a pagination request, matching
/// against a precise `from` token given as `expected_from`, and returning the
/// chunk of events and the next token as `end` (if available).
async fn mock_messages(
    server: &MockServer,
    expected_from: &str,
    next_token: Option<&str>,
    chunk: Vec<Raw<AnyTimelineEvent>>,
) {
    let response_json = json!({
        "chunk": chunk,
        "start": "t392-516_47314_0_7_1_1_1_11444_1",
        "end": next_token,
    });
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", expected_from))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_json))
        .expect(1)
        .mount(server)
        .await;
}

#[async_test]
async fn test_backpaginate_once() {
    let (client, server) = logged_in_client_with_server().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, and get a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();

    {
        sync_builder.add_joined_room(
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(event_builder.make_sync_message_event(
                    user_id!("@a:b.c"),
                    RoomMessageEventContent::text_plain("heyo"),
                ))
                .set_timeline_prev_batch("prev_batch".to_owned()),
        );
        let response_body = sync_builder.build_json_sync_response();

        mock_sync(&server, response_body, None).await;
        client.sync_once(Default::default()).await.unwrap();
        server.reset().await;
    }

    let (room_event_cache, _drop_handles) =
        client.get_room(room_id).unwrap().event_cache().await.unwrap();

    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    // This is racy: either the initial message has been processed by the event
    // cache (and no room updates will happen in this case), or it hasn't, and
    // the stream will return the next message soon.
    if events.is_empty() {
        let _ = room_stream.recv().await.expect("read error");
    } else {
        assert_eq!(events.len(), 1);
    }

    let outcome = {
        // Note: events must be presented in reversed order, since this is
        // back-pagination.
        mock_messages(
            &server,
            "prev_batch",
            None,
            non_sync_events!(event_builder, [ (room_id, "$2": "world"), (room_id, "$3": "hello") ]),
        )
        .await;

        // Then if I backpaginate,
        let token = room_event_cache
            .oldest_backpagination_token(Some(Duration::from_secs(1)))
            .await
            .unwrap();
        assert!(token.is_some());

        room_event_cache.backpaginate(20, token).await.unwrap()
    };

    // I'll get all the previous events, in "reverse" order (same as the response).
    assert_let!(BackPaginationOutcome::Success { events, reached_start } = outcome);
    assert!(reached_start);

    assert_event_matches_msg(&events[0].clone().into(), "world");
    assert_event_matches_msg(&events[1].clone().into(), "hello");
    assert_eq!(events.len(), 2);

    assert!(room_stream.is_empty());
}

#[async_test]
async fn test_backpaginate_multiple_iterations() {
    let (client, server) = logged_in_client_with_server().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, and get a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();

    {
        sync_builder.add_joined_room(
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(event_builder.make_sync_message_event(
                    user_id!("@a:b.c"),
                    RoomMessageEventContent::text_plain("heyo"),
                ))
                .set_timeline_prev_batch("prev_batch".to_owned()),
        );
        let response_body = sync_builder.build_json_sync_response();

        mock_sync(&server, response_body, None).await;
        client.sync_once(Default::default()).await.unwrap();
        server.reset().await;
    }

    let (room_event_cache, _drop_handles) =
        client.get_room(room_id).unwrap().event_cache().await.unwrap();

    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    // This is racy: either the initial message has been processed by the event
    // cache (and no room updates will happen in this case), or it hasn't, and
    // the stream will return the next message soon.
    if events.is_empty() {
        let _ = room_stream.recv().await.expect("read error");
    } else {
        assert_eq!(events.len(), 1);
    }

    let mut num_iterations = 0;
    let mut global_events = Vec::new();
    let mut global_reached_start = false;

    // The first back-pagination will return these two.
    mock_messages(
        &server,
        "prev_batch",
        Some("prev_batch2"),
        non_sync_events!(event_builder, [ (room_id, "$2": "world"), (room_id, "$3": "hello") ]),
    )
    .await;

    // The second round of back-pagination will return this one.
    mock_messages(
        &server,
        "prev_batch2",
        None,
        non_sync_events!(event_builder, [ (room_id, "$4": "oh well"), ]),
    )
    .await;

    // Then if I backpaginate in a loop,
    while let Some(token) =
        room_event_cache.oldest_backpagination_token(Some(Duration::from_secs(1))).await.unwrap()
    {
        match room_event_cache.backpaginate(20, Some(token)).await.unwrap() {
            BackPaginationOutcome::Success { reached_start, events } => {
                if !global_reached_start {
                    global_reached_start = reached_start;
                }
                global_events.extend(events);
            }
            BackPaginationOutcome::UnknownBackpaginationToken => {
                panic!("shouldn't run into unknown backpagination error")
            }
        }

        num_iterations += 1;
    }

    // I'll get all the previous events,
    assert_eq!(num_iterations, 2);
    assert!(global_reached_start);

    assert_event_matches_msg(&global_events[0].clone().into(), "world");
    assert_event_matches_msg(&global_events[1].clone().into(), "hello");
    assert_event_matches_msg(&global_events[2].clone().into(), "oh well");
    assert_eq!(global_events.len(), 3);

    // And next time I'll open the room, I'll get the events in the right order.
    let (events, _receiver) = room_event_cache.subscribe().await.unwrap();

    assert_event_matches_msg(&events[0], "oh well");
    assert_event_matches_msg(&events[1], "hello");
    assert_event_matches_msg(&events[2], "world");
    assert_event_matches_msg(&events[3], "heyo");
    assert_eq!(events.len(), 4);

    assert!(room_stream.is_empty());
}

#[async_test]
async fn test_reset_while_backpaginating() {
    let (client, server) = logged_in_client_with_server().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, and get a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();

    {
        sync_builder.add_joined_room(
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(event_builder.make_sync_message_event_with_id(
                    user_id!("@a:b.c"),
                    event_id!("$from_first_sync"),
                    RoomMessageEventContent::text_plain("heyo"),
                ))
                .set_timeline_prev_batch("first_prev_batch_token".to_owned()),
        );
        let response_body = sync_builder.build_json_sync_response();

        mock_sync(&server, response_body, None).await;
        client.sync_once(Default::default()).await.unwrap();
        server.reset().await;
    }

    let (room_event_cache, _drop_handles) =
        client.get_room(room_id).unwrap().event_cache().await.unwrap();

    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    // This is racy: either the initial message has been processed by the event
    // cache (and no room updates will happen in this case), or it hasn't, and
    // the stream will return the next message soon.
    if events.is_empty() {
        let _ = room_stream.recv().await.expect("read error");
    } else {
        assert_eq!(events.len(), 1);
    }

    // We're going to cause a small race:
    // - a background request to sync will be sent,
    // - a backpagination will be sent concurrently.
    //
    // So events have to happen in this order:
    // - the backpagination request is sent, with a prev-batch A
    // - the sync endpoint returns *after* the backpagination started, before the
    // backpagination ends
    // - the backpagination ends, with a prev-batch token that's now stale.
    //
    // The backpagination should result in an unknown-token-error.

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            // Note to self: a timeline must have at least single event to be properly
            // serialized.
            .add_timeline_event(event_builder.make_sync_message_event_with_id(
                user_id!("@a:b.c"),
                event_id!("$from_second_sync"),
                RoomMessageEventContent::text_plain("heyo"),
            ))
            .set_timeline_prev_batch("second_prev_batch_token_from_sync".to_owned())
            .set_timeline_limited(),
    );
    let sync_response_body = sync_builder.build_json_sync_response();

    // First back-pagination request:
    let chunk = non_sync_events!(event_builder, [ (room_id, "$from_backpagination": "lalala") ]);
    let response_json = json!({
        "chunk": chunk,
        "start": "t392-516_47314_0_7_1_1_1_11444_1",
        "end": "second_prev_batch_token_from_backpagination"
    });
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "first_prev_batch_token"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(response_json.clone())
                .set_delay(Duration::from_millis(500)), /* This is why we don't use
                                                         * `mock_messages`. */
        )
        .expect(1)
        .mount(&server)
        .await;

    let first_token =
        room_event_cache.oldest_backpagination_token(Some(Duration::from_secs(1))).await.unwrap();
    assert!(first_token.is_some());

    let rec = room_event_cache.clone();
    let first_token_clone = first_token.clone();
    let backpagination = spawn(async move { rec.backpaginate(20, first_token_clone).await });

    // Receive the sync response (which clears the timeline).
    mock_sync(&server, sync_response_body, None).await;
    client.sync_once(Default::default()).await.unwrap();

    let outcome = backpagination.await.expect("join failed");

    // Backpagination should be confused, and the operation should result in an
    // unknown token.
    assert_matches!(outcome, Ok(BackPaginationOutcome::UnknownBackpaginationToken));

    // Now if we retrieve the earliest token, it's not the one we had before.
    // Even better, it's the one from the sync, NOT from the backpagination!
    let second_token = room_event_cache.oldest_backpagination_token(None).await.unwrap().unwrap();
    assert!(first_token.unwrap() != second_token);
    assert_eq!(second_token.0, "second_prev_batch_token_from_sync");
}

#[async_test]
async fn test_backpaginating_without_token() {
    let (client, server) = logged_in_client_with_server().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, without a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let event_builder = EventBuilder::new();
    let mut sync_builder = SyncResponseBuilder::new();

    {
        sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        let response_body = sync_builder.build_json_sync_response();

        mock_sync(&server, response_body, None).await;
        client.sync_once(Default::default()).await.unwrap();
        server.reset().await;
    }

    let (room_event_cache, _drop_handles) =
        client.get_room(room_id).unwrap().event_cache().await.unwrap();

    let (events, room_stream) = room_event_cache.subscribe().await.unwrap();

    assert!(events.is_empty());
    assert!(room_stream.is_empty());

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": non_sync_events!(event_builder, [(room_id, "$2": "hi")]),
            "start": "t392-516_47314_0_7_1_1_1_11444_1",
        })))
        .expect(1)
        .mount(&server)
        .await;

    // We don't have a token.
    let token =
        room_event_cache.oldest_backpagination_token(Some(Duration::from_secs(1))).await.unwrap();
    assert!(token.is_none());

    // If we try to back-paginate with a token, it will hit the end of the timeline
    // and give us the resulting event.
    let outcome = room_event_cache.backpaginate(20, token).await.unwrap();
    assert_let!(BackPaginationOutcome::Success { events, reached_start } = outcome);

    assert!(reached_start);

    // And we get notified about the new event.
    assert_event_matches_msg(&events[0].clone().into(), "hi");
    assert_eq!(events.len(), 1);

    assert!(room_stream.is_empty());
}
