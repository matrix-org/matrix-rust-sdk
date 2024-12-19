use std::{future::ready, ops::ControlFlow, time::Duration};

use assert_matches::assert_matches;
use futures_util::FutureExt as _;
use matrix_sdk::{
    assert_let_timeout, assert_next_matches_with_timeout,
    deserialized_responses::SyncTimelineEvent,
    event_cache::{
        paginator::PaginatorState, BackPaginationOutcome, EventCacheError, RoomEventCacheUpdate,
        TimelineHasBeenResetWhilePaginating,
    },
    test_utils::{assert_event_matches_msg, mocks::MatrixMockServer},
};
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, GlobalAccountDataTestEvent, JoinedRoomBuilder,
};
use ruma::{event_id, events::AnyTimelineEvent, room_id, serde::Raw, user_id};
use serde_json::json;
use tokio::{spawn, sync::broadcast};
use wiremock::ResponseTemplate;

async fn once(
    outcome: BackPaginationOutcome,
    _timeline_has_been_reset: TimelineHasBeenResetWhilePaginating,
) -> ControlFlow<BackPaginationOutcome, ()> {
    ControlFlow::Break(outcome)
}

#[async_test]
async fn test_must_explicitly_subscribe() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!omelette:fromage.fr");

    // If I create a room event subscriber for a room before subscribing the event
    // cache,
    let room = server.sync_joined_room(&client, room_id).await;
    let result = room.event_cache().await;

    // Then it fails, because one must explicitly call `.subscribe()` on the event
    // cache.
    assert_matches!(result, Err(EventCacheError::NotSubscribedYet));
}

#[async_test]
async fn test_event_cache_receives_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Immediately subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    // If I sync and get informed I've joined The Room, but with no events,
    let room_id = room_id!("!omelette:fromage.fr");
    let room = server.sync_joined_room(&client, room_id).await;

    // If I create a room event subscriber,
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (events, mut subscriber) = room_event_cache.subscribe().await.unwrap();

    // Then at first it's empty, and the subscriber doesn't yield anything.
    assert!(events.is_empty());
    assert!(subscriber.is_empty());

    let f = EventFactory::new().sender(user_id!("@dexter:lab.org"));

    // And after a sync, yielding updates to two rooms,
    server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_joined_room(
                JoinedRoomBuilder::new(room_id).add_timeline_event(f.text_msg("bonjour monde")),
            );
            sync_builder.add_joined_room(
                JoinedRoomBuilder::new(room_id!("!parallel:universe.uk"))
                    .add_timeline_event(f.text_msg("hi i'm learning French")),
            );
        })
        .await;

    // It does receive one update,
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::AddTimelineEvents { events, .. }) = subscriber.recv()
    );
    // It does also receive the update as `VectorDiff`.
    assert_let_timeout!(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = subscriber.recv());

    // Which contains the event that was sent beforehand.
    assert_eq!(events.len(), 1);
    assert_event_matches_msg(&events[0], "bonjour monde");

    // That's all, folks!
    assert!(subscriber.is_empty());
}

#[async_test]
async fn test_ignored_unignored() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Immediately subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let other_room_id = room_id!("!galette:saucisse.bzh");

    let dexter = user_id!("@dexter:lab.org");
    let ivan = user_id!("@ivan:lab.ch");
    let f = EventFactory::new();

    // Given two known rooms with initial items,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                f.text_msg("hey there").sender(dexter).into_raw_sync(),
                f.text_msg("hoy!").sender(ivan).into_raw_sync(),
            ]),
        )
        .await;

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(other_room_id)
                .add_timeline_bulk(vec![f.text_msg("demat!").sender(ivan).into_raw_sync()]),
        )
        .await;

    // And subscribe to the room,
    let room = client.get_room(room_id).unwrap();
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (events, mut subscriber) = room_event_cache.subscribe().await.unwrap();

    // Then at first it contains the two initial events.
    assert_eq!(events.len(), 2);
    assert_event_matches_msg(&events[0], "hey there");
    assert_event_matches_msg(&events[1], "hoy!");

    // And after receiving a new ignored list,
    server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_global_account_data_event(GlobalAccountDataTestEvent::Custom(json!({
                "content": {
                    "ignored_users": {
                        dexter: {}
                    }
                },
                "type": "m.ignored_user_list",
            })));
        })
        .await;

    // It does receive one update, which notifies about the clear.
    assert_let_timeout!(Ok(RoomEventCacheUpdate::Clear) = subscriber.recv());

    // Receiving new events still works.
    server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(f.text_msg("i don't like this dexter").sender(ivan)),
            );
        })
        .await;

    // We do receive one update,
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::AddTimelineEvents { events, .. }) = subscriber.recv()
    );
    // It does also receive the update as `VectorDiff`.
    assert_let_timeout!(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = subscriber.recv());
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

/// Small helper for backpagination tests, to wait for things to stabilize.
async fn wait_for_initial_events(
    events: Vec<SyncTimelineEvent>,
    room_stream: &mut broadcast::Receiver<RoomEventCacheUpdate>,
) {
    if events.is_empty() {
        let mut update = room_stream.recv().await.expect("read error");
        // Could be a clear because of the limited timeline.
        if matches!(update, RoomEventCacheUpdate::Clear) {
            update = room_stream.recv().await.expect("read error");
        }
        assert_matches!(update, RoomEventCacheUpdate::AddTimelineEvents { .. });

        let update = room_stream.recv().await.expect("read error");

        assert_matches!(update, RoomEventCacheUpdate::UpdateTimelineEvents { .. });
    } else {
        assert_eq!(events.len(), 1);
    }
}

#[async_test]
async fn test_backpaginate_once() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    // If I sync and get informed I've joined The Room, and get a previous batch
    // token,
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(f.text_msg("heyo").event_id(event_id!("$1")))
                .set_timeline_prev_batch("prev_batch".to_owned())
                .set_timeline_limited(),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    // This is racy: either the initial message has been processed by the event
    // cache (and no room updates will happen in this case), or it hasn't, and
    // the stream will return the next message soon.
    wait_for_initial_events(events, &mut room_stream).await;

    let outcome = {
        // Note: events must be presented in reversed order, since this is
        // back-pagination.
        server
            .mock_room_messages()
            .from("prev_batch")
            .ok(
                "start-token-unused".to_owned(),
                None,
                vec![
                    f.text_msg("world").event_id(event_id!("$2")),
                    f.text_msg("hello").event_id(event_id!("$3")),
                ],
                Vec::new(),
            )
            .mock_once()
            .mount()
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

    let next = room_stream.recv().now_or_never();
    assert_matches!(next, Some(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. })));

    let next = room_stream.recv().now_or_never();
    assert_matches!(next, None);
}

#[async_test]
async fn test_backpaginate_many_times_with_many_iterations() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, and get a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(f.text_msg("heyo"))
                .set_timeline_prev_batch("prev_batch".to_owned())
                .set_timeline_limited(),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    // This is racy: either the initial message has been processed by the event
    // cache (and no room updates will happen in this case), or it hasn't, and
    // the stream will return the next message soon.
    wait_for_initial_events(events, &mut room_stream).await;

    let mut num_iterations = 0;
    let mut num_paginations = 0;
    let mut global_events = Vec::new();
    let mut global_reached_start = false;

    // The first back-pagination will return these two.
    server
        .mock_room_messages()
        .from("prev_batch")
        .ok(
            "start-token-unused".to_owned(),
            Some("prev_batch2".to_owned()),
            vec![
                f.text_msg("world").event_id(event_id!("$2")),
                f.text_msg("hello").event_id(event_id!("$3")),
            ],
            Vec::new(),
        )
        .mock_once()
        .mount()
        .await;

    // The second round of back-pagination will return this one.
    server
        .mock_room_messages()
        .from("prev_batch2")
        .ok(
            "start-token-unused".to_owned(),
            None,
            vec![f.text_msg("oh well").event_id(event_id!("$4"))],
            Vec::new(),
        )
        .mock_once()
        .mount()
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

    // First iteration.
    let next = room_stream.recv().now_or_never();
    assert_matches!(next, Some(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. })));

    // Second iteration.
    let next = room_stream.recv().now_or_never();
    assert_matches!(next, Some(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. })));

    assert!(room_stream.is_empty());
}

#[async_test]
async fn test_backpaginate_many_times_with_one_iteration() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, and get a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(f.text_msg("heyo"))
                .set_timeline_prev_batch("prev_batch".to_owned())
                .set_timeline_limited(),
        )
        .await;

    let (room_event_cache, _drop_handles) =
        client.get_room(room_id).unwrap().event_cache().await.unwrap();

    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    // This is racy: either the initial message has been processed by the event
    // cache (and no room updates will happen in this case), or it hasn't, and
    // the stream will return the next message soon.
    wait_for_initial_events(events, &mut room_stream).await;

    let mut num_iterations = 0;
    let mut num_paginations = 0;
    let mut global_events = Vec::new();
    let mut global_reached_start = false;

    // The first back-pagination will return these two.
    server
        .mock_room_messages()
        .from("prev_batch")
        .ok(
            "start-token-unused1".to_owned(),
            Some("prev_batch2".to_owned()),
            vec![
                f.text_msg("world").event_id(event_id!("$2")),
                f.text_msg("hello").event_id(event_id!("$3")),
            ],
            Vec::new(),
        )
        .mock_once()
        .mount()
        .await;

    // The second round of back-pagination will return this one.
    server
        .mock_room_messages()
        .from("prev_batch2")
        .ok(
            "start-token-unused2".to_owned(),
            None,
            vec![f.text_msg("oh well").event_id(event_id!("$4"))],
            Vec::new(),
        )
        .mock_once()
        .mount()
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

    // First pagination.
    let next = room_stream.recv().now_or_never();
    assert_matches!(next, Some(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. })));

    // Second pagination.
    let next = room_stream.recv().now_or_never();
    assert_matches!(next, Some(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. })));

    assert!(room_stream.is_empty());
}

#[async_test]
async fn test_reset_while_backpaginating() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, and get a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(f.text_msg("heyo").into_raw_sync())
                .set_timeline_prev_batch("first_backpagination".to_owned())
                .set_timeline_limited(),
        )
        .await;

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

    // Mock the first back-pagination request, with a delay.
    server
        .mock_room_messages()
        .from("first_backpagination")
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "chunk": vec![f.text_msg("lalala").into_raw_timeline()],
                    "start": "t392-516_47314_0_7_1_1_1_11444_1",
                }))
                // This is why we don't use `server.mock_room_messages()`.
                .set_delay(Duration::from_millis(500)),
        )
        .mock_once()
        .mount()
        .await;

    // Mock the second back-pagination request, that will be hit after the reset
    // caused by the sync.
    server
        .mock_room_messages()
        .from("second_backpagination")
        .ok(
            "start-token-unused".to_owned(),
            Some("third_backpagination".to_owned()),
            vec![f.text_msg("finally!").into_raw_timeline()],
            Vec::new(),
        )
        .mock_once()
        .mount()
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
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                // Note to self: a timeline must have at least single event to be properly
                // serialized.
                .add_timeline_event(f.text_msg("heyo").into_raw_sync())
                .set_timeline_prev_batch("second_backpagination".to_owned())
                .set_timeline_limited(),
        )
        .await;

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
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, without a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));
    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    assert!(events.is_empty());
    assert!(room_stream.is_empty());

    server
        .mock_room_messages()
        .ok(
            "start-token-unused".to_owned(),
            None,
            vec![f.text_msg("hi").event_id(event_id!("$2")).into_raw_timeline()],
            Vec::new(),
        )
        .mock_once()
        .mount()
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

    let next = room_stream.recv().now_or_never();
    assert_matches!(next, Some(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. })));

    assert!(room_stream.is_empty());
}

#[async_test]
async fn test_limited_timeline_resets_pagination() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    // If I sync and get informed I've joined The Room, without a previous batch
    // token,
    let room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));
    let room = server.sync_joined_room(&client, room_id).await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    assert!(events.is_empty());
    assert!(room_stream.is_empty());

    server
        .mock_room_messages()
        .ok(
            "start-token-unused".to_owned(),
            None,
            vec![f.text_msg("hi").event_id(event_id!("$2")).into_raw_timeline()],
            Vec::new(),
        )
        .mock_once()
        .mount()
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
    assert_next_matches_with_timeout!(pagination_status, PaginatorState::Idle);
    assert!(pagination.hit_timeline_start());

    // When a limited sync comes back from the server,
    server.sync_room(&client, JoinedRoomBuilder::new(room_id).set_timeline_limited()).await;

    // We receive an update about the limited timeline.
    assert_let_timeout!(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = room_stream.recv());
    assert_let_timeout!(Ok(RoomEventCacheUpdate::Clear) = room_stream.recv());

    // The paginator state is reset: status set to Initial, hasn't hit the timeline
    // start.
    assert!(!pagination.hit_timeline_start());
    assert_eq!(pagination_status.get(), PaginatorState::Initial);

    // We receive an update about the paginator status.
    assert_next_matches_with_timeout!(pagination_status, PaginatorState::Initial);

    assert!(room_stream.is_empty());
}

#[async_test]
async fn test_limited_timeline_with_storage() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Don't forget to subscribe and like^W enable storage!
    event_cache.subscribe().unwrap();
    event_cache.enable_storage().unwrap();

    let room_id = room_id!("!galette:saucisse.bzh");
    let room = server.sync_joined_room(&client, room_id).await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

    // First sync: get a message.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(f.text_msg("hey yo")),
        )
        .await;

    let (initial_events, mut subscriber) = room_event_cache.subscribe().await.unwrap();

    // This is racy: either the sync has been handled, or it hasn't yet.
    if initial_events.is_empty() {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::AddTimelineEvents { events, .. }) = subscriber.recv()
        );
        // It does also receive the update as `VectorDiff`.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = subscriber.recv()
        );
        assert_eq!(events.len(), 1);
        assert_event_matches_msg(&events[0], "hey yo");
    } else {
        assert_eq!(initial_events.len(), 1);
        assert_event_matches_msg(&initial_events[0], "hey yo");
    }

    assert!(subscriber.is_empty());

    // Second update: get a message but from a limited timeline.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("gappy!"))
                .set_timeline_limited(),
        )
        .await;

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::AddTimelineEvents { events, .. }) = subscriber.recv()
    );
    // It does also receive the update as `VectorDiff`.
    assert_let_timeout!(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = subscriber.recv());
    assert_eq!(events.len(), 1);
    assert_event_matches_msg(&events[0], "gappy!");

    // That's all, folks!
    assert!(subscriber.is_empty());
}

#[async_test]
async fn test_backpaginate_with_no_initial_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    // Start with a room with an event, but no prev-batch token.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").event_id(event_id!("$3"))),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (events, mut stream) = room_event_cache.subscribe().await.unwrap();
    wait_for_initial_events(events, &mut stream).await;

    // The first back-pagination will return these two events.
    //
    // Note: it's important to return the same event that came from sync: since we
    // will back-paginate without a prev-batch token first, we'll back-paginate
    // from the end of the timeline, which must include the event we got from
    // sync.

    server
        .mock_room_messages()
        .ok(
            "start-token-unused1".to_owned(),
            Some("prev_batch".to_owned()),
            vec![
                f.text_msg("world").event_id(event_id!("$2")),
                f.text_msg("hello").event_id(event_id!("$3")),
            ],
            Vec::new(),
        )
        .mock_once()
        .mount()
        .await;

    // The second round of back-pagination will return this one.
    server
        .mock_room_messages()
        .from("prev_batch")
        .ok(
            "start-token-unused2".to_owned(),
            None,
            vec![f.text_msg("oh well").event_id(event_id!("$1"))],
            Vec::new(),
        )
        .mock_once()
        .mount()
        .await;

    let pagination = room_event_cache.pagination();

    // Run pagination: since there's no token, we'll wait a bit for a sync to return
    // one, and since there's none, we'll end up starting from the end of the
    // timeline.
    pagination.run_backwards(20, once).await.unwrap();
    // Second pagination will be instant.
    pagination.run_backwards(20, once).await.unwrap();

    // The linked chunk should contain the events in the correct order.
    let (events, _stream) = room_event_cache.subscribe().await.unwrap();

    assert_event_matches_msg(&events[0], "oh well");
    assert_event_matches_msg(&events[1], "hello");
    assert_event_matches_msg(&events[2], "world");
    assert_eq!(events.len(), 3);
}

#[async_test]
async fn test_backpaginate_replace_empty_gap() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    // Start with a room with an event, limited timeline and prev-batch token.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("world").event_id(event_id!("$2")))
                .set_timeline_limited()
                .set_timeline_prev_batch("prev-batch".to_owned()),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (events, mut stream) = room_event_cache.subscribe().await.unwrap();
    wait_for_initial_events(events, &mut stream).await;

    // The first back-pagination will return a previous-batch token, but no events.
    server
        .mock_room_messages()
        .ok(
            "start-token-unused1".to_owned(),
            Some("prev_batch".to_owned()),
            Vec::<Raw<AnyTimelineEvent>>::new(),
            Vec::new(),
        )
        .mock_once()
        .mount()
        .await;

    // The second round of back-pagination will return this one.
    server
        .mock_room_messages()
        .from("prev_batch")
        .ok(
            "start-token-unused2".to_owned(),
            None,
            vec![f.text_msg("hello").event_id(event_id!("$1"))],
            Vec::new(),
        )
        .mock_once()
        .mount()
        .await;

    let pagination = room_event_cache.pagination();

    // Run pagination twice.
    pagination.run_backwards(20, once).await.unwrap();
    pagination.run_backwards(20, once).await.unwrap();

    // The linked chunk should contain the events in the correct order.
    let (events, _stream) = room_event_cache.subscribe().await.unwrap();

    assert_event_matches_msg(&events[0], "hello");
    assert_event_matches_msg(&events[1], "world");
    assert_eq!(events.len(), 2);
}
