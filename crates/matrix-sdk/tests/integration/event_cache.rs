use std::{future::ready, ops::ControlFlow, time::Duration};

use assert_matches2::{assert_let, assert_matches};
use matrix_sdk::{
    event_cache::{
        paginator::PaginatorState, BackPaginationOutcome, EventCacheError, RoomEventCacheUpdate,
        TimelineHasBeenResetWhilePaginating,
    },
    test_utils::{assert_event_matches_msg, logged_in_client_with_server},
};
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, GlobalAccountDataTestEvent, JoinedRoomBuilder,
    SyncResponseBuilder,
};
use ruma::{event_id, events::AnyTimelineEvent, room_id, serde::Raw, user_id};
use serde_json::json;
use tokio::{spawn, time::timeout};
use wiremock::{
    matchers::{header, method, path_regex, query_param},
    Mock, MockServer, ResponseTemplate,
};

use crate::mock_sync;

async fn once(
    outcome: BackPaginationOutcome,
    _timeline_has_been_reset: TimelineHasBeenResetWhilePaginating,
) -> ControlFlow<BackPaginationOutcome, ()> {
    ControlFlow::Break(outcome)
}

#[async_test]
async fn test_must_explicitly_subscribe() {
    let (client, server) = logged_in_client_with_server().await;

    let room_id = room_id!("!omelette:fromage.fr");

    {
        // Make sure the client is aware of the room.
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

    let ev_factory = EventFactory::new().sender(user_id!("@dexter:lab.org"));

    // And after a sync, yielding updates to two rooms,
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(ev_factory.text_msg("bonjour monde")),
    );

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id!("!parallel:universe.uk"))
            .add_timeline_event(ev_factory.text_msg("hi i'm learning French")),
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
    assert_let!(RoomEventCacheUpdate::AddTimelineEvents { events, .. } = update);
    assert_eq!(events.len(), 1);
    assert_event_matches_msg(&events[0], "bonjour monde");

    // And when I later add initial events to this room,

    // XXX: when we get rid of `add_initial_events`, we can keep this test as a
    // smoke test for the event cache.
    client
        .event_cache()
        .add_initial_events(room_id, vec![ev_factory.text_msg("new choice!").into_sync()], None)
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
    assert_let!(RoomEventCacheUpdate::AddTimelineEvents { events, .. } = update);
    assert_eq!(events.len(), 1);
    assert_event_matches_msg(&events[0], "new choice!");

    // That's all, folks!
    assert!(subscriber.is_empty());
}

#[async_test]
async fn test_ignored_unignored() {
    let (client, server) = logged_in_client_with_server().await;

    // Immediately subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    // If I sync and get informed I've joined The Room, but with no events,
    let room_id = room_id!("!omelette:fromage.fr");
    let other_room_id = room_id!("!galette:saucisse.bzh");

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder
        .add_joined_room(JoinedRoomBuilder::new(room_id))
        .add_joined_room(JoinedRoomBuilder::new(other_room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(Default::default()).await.unwrap();
    server.reset().await;

    let dexter = user_id!("@dexter:lab.org");
    let ivan = user_id!("@ivan:lab.ch");
    let ev_factory = EventFactory::new();

    // If I add initial events to a few rooms,
    client
        .event_cache()
        .add_initial_events(
            room_id,
            vec![
                ev_factory.text_msg("hey there").sender(dexter).into_sync(),
                ev_factory.text_msg("hoy!").sender(ivan).into_sync(),
            ],
            None,
        )
        .await
        .unwrap();

    client
        .event_cache()
        .add_initial_events(
            other_room_id,
            vec![ev_factory.text_msg("demat!").sender(ivan).into_sync()],
            None,
        )
        .await
        .unwrap();

    // And subscribe to the room,
    let room = client.get_room(room_id).unwrap();
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (events, mut subscriber) = room_event_cache.subscribe().await.unwrap();

    // Then at first it contains the two initial events.
    assert_eq!(events.len(), 2);
    assert_event_matches_msg(&events[0], "hey there");
    assert_event_matches_msg(&events[1], "hoy!");

    // And after receiving a new ignored list,
    sync_builder.add_global_account_data_event(GlobalAccountDataTestEvent::Custom(json!({
        "content": {
            "ignored_users": {
                dexter: {}
            }
        },
        "type": "m.ignored_user_list",
    })));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(Default::default()).await.unwrap();
    server.reset().await;

    // It does receive one update,
    let update = timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("timeout after receiving a sync update")
        .expect("should've received a room event cache update");

    // Which notifies about the clear.
    assert_matches!(update, RoomEventCacheUpdate::Clear);

    // Receiving new events still works.
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(ev_factory.text_msg("i don't like this dexter").sender(ivan)),
    );
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    client.sync_once(Default::default()).await.unwrap();
    server.reset().await;

    // We do receive one update,
    let update = timeout(Duration::from_secs(2), subscriber.recv())
        .await
        .expect("timeout after receiving a sync update")
        .expect("should've received a room event cache update");

    assert_let!(RoomEventCacheUpdate::AddTimelineEvents { events, .. } = update);
    assert_eq!(events.len(), 1);
    assert_event_matches_msg(&events[0], "i don't like this dexter");

    // The other room has been cleared too.
    {
        let room = client.get_room(other_room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        let (events, _) = room_event_cache.subscribe().await.unwrap();
        assert!(events.is_empty());
    }

    // That's all, folks!
    assert!(subscriber.is_empty());
}

/// Puts a mounting point for /messages for a pagination request, matching
/// against a precise `from` token given as `expected_from`, and returning the
/// chunk of events and the next token as `end` (if available).
// TODO: replace this with the `mock_room_messages` form mocks.rs
async fn mock_messages(
    server: &MockServer,
    expected_from: &str,
    next_token: Option<&str>,
    chunk: Vec<impl Into<Raw<AnyTimelineEvent>>>,
) {
    let response_json = json!({
        "chunk": chunk.into_iter().map(|item| item.into()).collect::<Vec<_>>(),
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

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));
    let mut sync_builder = SyncResponseBuilder::new();

    {
        sync_builder.add_joined_room(
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(f.text_msg("heyo"))
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
            vec![
                f.text_msg("world").event_id(event_id!("$2")),
                f.text_msg("hello").event_id(event_id!("$3")),
            ],
        )
        .await;

        // Then if I backpaginate,
        let pagination = room_event_cache.pagination();

        assert!(pagination.get_or_wait_for_token(None).await.is_some());

        pagination.run_backwards(20, once).await.unwrap()
    };

    // I'll get all the previous events, in "reverse" order (same as the response).
    let BackPaginationOutcome { events, reached_start } = outcome;
    assert!(reached_start);

    assert_event_matches_msg(&events[0], "world");
    assert_event_matches_msg(&events[1], "hello");
    assert_eq!(events.len(), 2);

    assert!(room_stream.is_empty());
}

#[async_test]
async fn test_backpaginate_many_times_with_many_iterations() {
    let (client, server) = logged_in_client_with_server().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, and get a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));
    let mut sync_builder = SyncResponseBuilder::new();

    {
        sync_builder.add_joined_room(
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(f.text_msg("heyo"))
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
    let mut num_paginations = 0;
    let mut global_events = Vec::new();
    let mut global_reached_start = false;

    // The first back-pagination will return these two.
    mock_messages(
        &server,
        "prev_batch",
        Some("prev_batch2"),
        vec![
            f.text_msg("world").event_id(event_id!("$2")),
            f.text_msg("hello").event_id(event_id!("$3")),
        ],
    )
    .await;

    // The second round of back-pagination will return this one.
    mock_messages(
        &server,
        "prev_batch2",
        None,
        vec![f.text_msg("oh well").event_id(event_id!("$4"))],
    )
    .await;

    // Then if I backpaginate in a loop,
    let pagination = room_event_cache.pagination();
    while pagination.get_or_wait_for_token(None).await.is_some() {
        pagination
            .run_backwards(20, |outcome, timeline_has_been_reset| {
                num_paginations += 1;

                assert_matches!(timeline_has_been_reset, TimelineHasBeenResetWhilePaginating::No);

                if !global_reached_start {
                    global_reached_start = outcome.reached_start;
                }

                global_events.extend(outcome.events);

                ready(ControlFlow::Break(()))
            })
            .await
            .unwrap();

        num_iterations += 1;
    }

    // I'll get all the previous events,
    assert_eq!(num_iterations, 2); // in two iterations…
    assert_eq!(num_paginations, 2); // … we get two paginations.
    assert!(global_reached_start);

    assert_event_matches_msg(&global_events[0], "world");
    assert_event_matches_msg(&global_events[1], "hello");
    assert_event_matches_msg(&global_events[2], "oh well");
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
async fn test_backpaginate_many_times_with_one_iteration() {
    let (client, server) = logged_in_client_with_server().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, and get a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));
    let mut sync_builder = SyncResponseBuilder::new();

    {
        sync_builder.add_joined_room(
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(f.text_msg("heyo"))
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
    let mut num_paginations = 0;
    let mut global_events = Vec::new();
    let mut global_reached_start = false;

    // The first back-pagination will return these two.
    mock_messages(
        &server,
        "prev_batch",
        Some("prev_batch2"),
        vec![
            f.text_msg("world").event_id(event_id!("$2")),
            f.text_msg("hello").event_id(event_id!("$3")),
        ],
    )
    .await;

    // The second round of back-pagination will return this one.
    mock_messages(
        &server,
        "prev_batch2",
        None,
        vec![f.text_msg("oh well").event_id(event_id!("$4"))],
    )
    .await;

    // Then if I backpaginate in a loop,
    let pagination = room_event_cache.pagination();
    while pagination.get_or_wait_for_token(None).await.is_some() {
        pagination
            .run_backwards(20, |outcome, timeline_has_been_reset| {
                num_paginations += 1;

                assert_matches!(timeline_has_been_reset, TimelineHasBeenResetWhilePaginating::No);

                if !global_reached_start {
                    global_reached_start = outcome.reached_start;
                }

                global_events.extend(outcome.events);

                ready(if outcome.reached_start {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                })
            })
            .await
            .unwrap();

        num_iterations += 1;
    }

    // I'll get all the previous events,
    assert_eq!(num_iterations, 1); // in one iteration…
    assert_eq!(num_paginations, 2); // … we get two paginations!
    assert!(global_reached_start);

    assert_event_matches_msg(&global_events[0], "world");
    assert_event_matches_msg(&global_events[1], "hello");
    assert_event_matches_msg(&global_events[2], "oh well");
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

    let ev_factory = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));
    let mut sync_builder = SyncResponseBuilder::new();

    {
        sync_builder.add_joined_room(
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(ev_factory.text_msg("heyo").into_raw_sync())
                .set_timeline_prev_batch("first_backpagination".to_owned()),
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
            .add_timeline_event(ev_factory.text_msg("heyo").into_raw_sync())
            .set_timeline_prev_batch("second_backpagination".to_owned())
            .set_timeline_limited(),
    );
    let sync_response_body = sync_builder.build_json_sync_response();

    // Mock the first back-pagination request:
    let chunk = vec![ev_factory.text_msg("lalala").into_raw_timeline()];
    let response_json = json!({
        "chunk": chunk,
        "start": "t392-516_47314_0_7_1_1_1_11444_1",
    });
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", "first_backpagination"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(response_json.clone())
                .set_delay(Duration::from_millis(500)), /* This is why we don't use
                                                         * `mock_messages`. */
        )
        .expect(1)
        .mount(&server)
        .await;

    // Mock the second back-pagination request, that will be hit after the reset
    // caused by the sync.
    mock_messages(
        &server,
        "second_backpagination",
        Some("third_backpagination"),
        vec![ev_factory.text_msg("finally!").into_raw_timeline()],
    )
    .await;

    // Run the pagination!
    let pagination = room_event_cache.pagination();

    let first_token = pagination.get_or_wait_for_token(None).await;
    assert!(first_token.is_some());

    let backpagination = spawn({
        let pagination = room_event_cache.pagination();
        async move {
            pagination
                .run_backwards(20, |outcome, timeline_has_been_reset| {
                    assert_matches!(
                        timeline_has_been_reset,
                        TimelineHasBeenResetWhilePaginating::Yes
                    );

                    ready(ControlFlow::Break(outcome))
                })
                .await
        }
    });

    // Receive the sync response (which clears the timeline).
    mock_sync(&server, sync_response_body, None).await;
    client.sync_once(Default::default()).await.unwrap();

    let outcome = backpagination.await.expect("join failed").unwrap();

    // Backpagination will automatically restart, so eventually we get the events.
    let BackPaginationOutcome { events, .. } = outcome;
    assert!(!events.is_empty());

    // Now if we retrieve the oldest token, it's set to something else.
    let second_token = pagination.get_or_wait_for_token(None).await.unwrap();
    assert!(first_token.unwrap() != second_token);
    assert_eq!(second_token, "third_backpagination");
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

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));
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
            "chunk": vec![
                f.text_msg("hi").event_id(event_id!("$2")).into_raw_timeline()
            ],
            "start": "t392-516_47314_0_7_1_1_1_11444_1",
        })))
        .expect(1)
        .mount(&server)
        .await;

    // We don't have a token.
    let pagination = room_event_cache.pagination();
    assert!(pagination.get_or_wait_for_token(None).await.is_none());

    // If we try to back-paginate with a token, it will hit the end of the timeline
    // and give us the resulting event.
    let BackPaginationOutcome { events, reached_start } =
        pagination.run_backwards(20, once).await.unwrap();

    assert!(reached_start);

    // And we get notified about the new event.
    assert_event_matches_msg(&events[0], "hi");
    assert_eq!(events.len(), 1);

    assert!(room_stream.is_empty());
}

#[async_test]
async fn test_limited_timeline_resets_pagination() {
    let (client, server) = logged_in_client_with_server().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, without a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));
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

    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    assert!(events.is_empty());
    assert!(room_stream.is_empty());

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "chunk": vec![
                f.text_msg("hi").event_id(event_id!("$2")).into_raw_timeline()
            ],
            "start": "t392-516_47314_0_7_1_1_1_11444_1",
        })))
        .expect(1)
        .mount(&server)
        .await;

    // At the beginning, the paginator is in the initial state.
    let pagination = room_event_cache.pagination();
    let mut pagination_status = pagination.status();
    assert_eq!(pagination_status.get(), PaginatorState::Initial);

    // If we try to back-paginate with a token, it will hit the end of the timeline
    // and give us the resulting event.
    let BackPaginationOutcome { events, reached_start } =
        pagination.run_backwards(20, once).await.unwrap();

    assert_eq!(events.len(), 1);
    assert!(reached_start);

    // And the paginator state delives this as an update, and is internally
    // consistent with it:
    assert_eq!(
        timeout(Duration::from_secs(1), pagination_status.next()).await,
        Ok(Some(PaginatorState::Idle))
    );
    assert!(pagination.hit_timeline_start());

    // When a limited sync comes back from the server,
    {
        sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).set_timeline_limited());
        let response_body = sync_builder.build_json_sync_response();

        mock_sync(&server, response_body, None).await;
        client.sync_once(Default::default()).await.unwrap();
        server.reset().await;
    }

    // We receive an update about the limited timeline.
    assert_matches!(
        timeout(Duration::from_secs(1), room_stream.recv()).await,
        Ok(Ok(RoomEventCacheUpdate::Clear))
    );

    // The paginator state is reset: status set to Initial, hasn't hit the timeline
    // start.
    assert!(!pagination.hit_timeline_start());
    assert_eq!(pagination_status.get(), PaginatorState::Initial);

    // We receive an update about the paginator status.
    let next_state = timeout(Duration::from_secs(1), pagination_status.next())
        .await
        .expect("timeout")
        .expect("no update");
    assert_eq!(next_state, PaginatorState::Initial);

    assert!(room_stream.is_empty());
}
