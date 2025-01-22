use std::{
    future::ready,
    ops::{ControlFlow, Not},
    time::Duration,
};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk::{
    assert_let_timeout, assert_next_matches_with_timeout,
    deserialized_responses::TimelineEvent,
    event_cache::{
        paginator::PaginatorState, BackPaginationOutcome, EventCacheError, PaginationToken,
        RoomEventCacheUpdate, TimelineHasBeenResetWhilePaginating,
    },
    test_utils::{
        assert_event_matches_msg,
        mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    },
};
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, GlobalAccountDataTestEvent, JoinedRoomBuilder,
};
use ruma::{
    event_id,
    events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent},
    room_id, user_id, RoomVersionId,
};
use serde_json::json;
use tokio::{spawn, sync::broadcast, time::sleep};

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
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );
    assert_eq!(diffs.len(), 1);
    assert_let!(VectorDiff::Append { values: events } = &diffs[0]);

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
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );
    assert_eq!(diffs.len(), 2);

    // Similar to the `RoomEventCacheUpdate::Clear`.
    assert_let!(VectorDiff::Clear = &diffs[0]);
    assert_let!(VectorDiff::Append { values: events } = &diffs[1]);
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
    events: Vec<TimelineEvent>,
    room_stream: &mut broadcast::Receiver<RoomEventCacheUpdate>,
) {
    if events.is_empty() {
        assert_let_timeout!(Ok(update) = room_stream.recv());
        let mut update = update;

        // Could be a clear because of the limited timeline.
        if matches!(update, RoomEventCacheUpdate::Clear) {
            assert_let_timeout!(Ok(new_update) = room_stream.recv());
            update = new_update;
        }

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
            .ok(RoomMessagesResponseTemplate::default().events(vec![
                f.text_msg("world").event_id(event_id!("$2")),
                f.text_msg("hello").event_id(event_id!("$3")),
            ]))
            .mock_once()
            .mount()
            .await;

        // Then if I backpaginate,
        let pagination = room_event_cache.pagination();

        assert_matches!(pagination.get_or_wait_for_token(None).await, PaginationToken::HasMore(_));

        pagination.run_backwards(20, once).await.unwrap()
    };

    // I'll get all the previous events, in "reverse" order (same as the response).
    let BackPaginationOutcome { events, reached_start } = outcome;
    assert!(reached_start);

    assert_eq!(events.len(), 2);
    assert_event_matches_msg(&events[0], "world");
    assert_event_matches_msg(&events[1], "hello");

    // And we get update as diffs.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );

    assert_eq!(diffs.len(), 2);
    assert_matches!(&diffs[0], VectorDiff::Insert { index, value: event } => {
        assert_eq!(*index, 0);
        assert_event_matches_msg(event, "hello");
    });
    assert_matches!(&diffs[1], VectorDiff::Insert { index, value: event } => {
        assert_eq!(*index, 1);
        assert_event_matches_msg(event, "world");
    });

    assert!(room_stream.is_empty());
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
        .ok(RoomMessagesResponseTemplate::default().end_token("prev_batch2").events(vec![
            f.text_msg("world").event_id(event_id!("$2")),
            f.text_msg("hello").event_id(event_id!("$3")),
        ]))
        .mock_once()
        .mount()
        .await;

    // The second round of back-pagination will return this one.
    server
        .mock_room_messages()
        .from("prev_batch2")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("oh well").event_id(event_id!("$4"))]))
        .mock_once()
        .mount()
        .await;

    // Then if I backpaginate in a loop,
    let pagination = room_event_cache.pagination();
    while matches!(pagination.get_or_wait_for_token(None).await, PaginationToken::HasMore(_)) {
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

    // First iteration.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );

    assert_eq!(diffs.len(), 2);
    assert_matches!(&diffs[0], VectorDiff::Insert { index, value: event } => {
        assert_eq!(*index, 0);
        assert_event_matches_msg(event, "hello");
    });
    assert_matches!(&diffs[1], VectorDiff::Insert { index, value: event } => {
        assert_eq!(*index, 1);
        assert_event_matches_msg(event, "world");
    });

    // Second iteration.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );

    assert_eq!(diffs.len(), 1);
    assert_matches!(&diffs[0], VectorDiff::Insert { index, value: event } => {
        assert_eq!(*index, 0);
        assert_event_matches_msg(event, "oh well");
    });

    assert!(room_stream.is_empty());

    // And next time I'll open the room, I'll get the events in the right order.
    let (events, room_stream) = room_event_cache.subscribe().await.unwrap();

    assert_event_matches_msg(&events[0], "oh well");
    assert_event_matches_msg(&events[1], "hello");
    assert_event_matches_msg(&events[2], "world");
    assert_event_matches_msg(&events[3], "heyo");
    assert_eq!(events.len(), 4);

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
        .ok(RoomMessagesResponseTemplate::default().end_token("prev_batch2").events(vec![
            f.text_msg("world").event_id(event_id!("$2")),
            f.text_msg("hello").event_id(event_id!("$3")),
        ]))
        .mock_once()
        .mount()
        .await;

    // The second round of back-pagination will return this one.
    server
        .mock_room_messages()
        .from("prev_batch2")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("oh well").event_id(event_id!("$4"))]))
        .mock_once()
        .mount()
        .await;

    // Then if I backpaginate in a loop,
    let pagination = room_event_cache.pagination();
    while matches!(pagination.get_or_wait_for_token(None).await, PaginationToken::HasMore(_)) {
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

    // First pagination.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );

    assert_eq!(diffs.len(), 2);
    assert_matches!(&diffs[0], VectorDiff::Insert { index, value: event } => {
        assert_eq!(*index, 0);
        assert_event_matches_msg(event, "hello");
    });
    assert_matches!(&diffs[1], VectorDiff::Insert { index, value: event } => {
        assert_eq!(*index, 1);
        assert_event_matches_msg(event, "world");
    });

    // Second pagination.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );

    assert_eq!(diffs.len(), 1);
    assert_matches!(&diffs[0], VectorDiff::Insert { index, value: event } => {
        assert_eq!(*index, 0);
        assert_event_matches_msg(event, "oh well");
    });

    // And next time I'll open the room, I'll get the events in the right order.
    let (events, room_stream) = room_event_cache.subscribe().await.unwrap();

    assert_event_matches_msg(&events[0], "oh well");
    assert_event_matches_msg(&events[1], "hello");
    assert_event_matches_msg(&events[2], "world");
    assert_event_matches_msg(&events[3], "heyo");
    assert_eq!(events.len(), 4);

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
        assert_let_timeout!(Ok(RoomEventCacheUpdate::Clear) = room_stream.recv());
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = room_stream.recv()
        );
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
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("lalala").into_raw_timeline()])
            .delayed(Duration::from_millis(500)))
        .mock_once()
        .mount()
        .await;

    // Mock the second back-pagination request, that will be hit after the reset
    // caused by the sync.
    server
        .mock_room_messages()
        .from("second_backpagination")
        .ok(RoomMessagesResponseTemplate::default()
            .end_token("third_backpagination")
            .events(vec![f.text_msg("finally!").into_raw_timeline()]))
        .mock_once()
        .mount()
        .await;

    // Run the pagination!
    let pagination = room_event_cache.pagination();

    assert_let!(
        PaginationToken::HasMore(first_token) = pagination.get_or_wait_for_token(None).await
    );

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
    assert_let!(
        PaginationToken::HasMore(second_token) = pagination.get_or_wait_for_token(None).await
    );
    assert!(first_token != second_token);
    assert_eq!(second_token, "third_backpagination");

    // Assert the updates as diffs.

    // Being cleared from the reset.
    assert_let_timeout!(Ok(RoomEventCacheUpdate::Clear) = room_stream.recv());

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );
    assert_eq!(diffs.len(), 2);
    // The clear, again.
    assert_matches!(&diffs[0], VectorDiff::Clear);
    // The event from the sync.
    assert_matches!(&diffs[1], VectorDiff::Append { values: events } => {
        assert_eq!(events.len(), 1);
        assert_event_matches_msg(&events[0], "heyo");
    });

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );
    assert_eq!(diffs.len(), 1);
    // The event from the pagination.
    assert_matches!(&diffs[0], VectorDiff::Insert { index, value: event } => {
        assert_eq!(*index, 0);
        assert_event_matches_msg(event, "finally!");
    });

    assert!(room_stream.is_empty());
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
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("hi").event_id(event_id!("$2")).into_raw_timeline()]))
        .mock_once()
        .mount()
        .await;

    // We don't have a token.
    let pagination = room_event_cache.pagination();
    assert_eq!(pagination.get_or_wait_for_token(None).await, PaginationToken::None);

    // If we try to back-paginate with a token, it will hit the end of the timeline
    // and give us the resulting event.
    let BackPaginationOutcome { events, reached_start } =
        pagination.run_backwards(20, once).await.unwrap();

    assert!(reached_start);

    // And we get notified about the new event.
    assert_event_matches_msg(&events[0], "hi");
    assert_eq!(events.len(), 1);

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );
    assert_eq!(diffs.len(), 1);
    assert_matches!(&diffs[0], VectorDiff::Append { values: events } => {
        assert_eq!(events.len(), 1);
        assert_event_matches_msg(&events[0], "hi");
    });

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
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("hi").event_id(event_id!("$2")).into_raw_timeline()]))
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

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );
    assert_eq!(diffs.len(), 1);
    assert_matches!(&diffs[0], VectorDiff::Append { values: events } => {
        assert_eq!(events.len(), 1);
        assert_event_matches_msg(&events[0], "hi");
    });

    // And the paginator state delives this as an update, and is internally
    // consistent with it:
    assert_next_matches_with_timeout!(pagination_status, PaginatorState::Idle);
    assert!(pagination.hit_timeline_start());

    // When a limited sync comes back from the server,
    server.sync_room(&client, JoinedRoomBuilder::new(room_id).set_timeline_limited()).await;

    // We receive an update about the limited timeline.
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
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
        );
        assert_eq!(diffs.len(), 1);

        assert_let!(VectorDiff::Append { values: events } = &diffs[0]);
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
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );
    assert_eq!(diffs.len(), 1);

    assert_let!(VectorDiff::Append { values: events } = &diffs[0]);
    assert_eq!(events.len(), 1);
    assert_event_matches_msg(&events[0], "gappy!");

    // That's all, folks!
    assert!(subscriber.is_empty());
}

#[async_test]
async fn test_limited_timeline_without_storage() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    event_cache.subscribe().unwrap();

    let room_id = room_id!("!galette:saucisse.bzh");
    let room = server.sync_joined_room(&client, room_id).await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

    // Get a sync for a non-limited timeline, but with a prev-batch token.
    //
    // When we don't have storage, we should still keep this prev-batch around,
    // because the previous sync events may not have been saved to disk in the
    // first place.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hey yo"))
                .set_timeline_prev_batch("prev-batch".to_owned()),
        )
        .await;

    let (initial_events, mut subscriber) = room_event_cache.subscribe().await.unwrap();

    // This is racy: either the sync has been handled, or it hasn't yet.
    if initial_events.is_empty() {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
        );
        assert_eq!(diffs.len(), 1);

        assert_let!(VectorDiff::Append { values: events } = &diffs[0]);
        assert_eq!(events.len(), 1);
        assert_event_matches_msg(&events[0], "hey yo");
    } else {
        assert_eq!(initial_events.len(), 1);
        assert_event_matches_msg(&initial_events[0], "hey yo");
    }

    // Back-pagination should thus work. The real assertion is in the "mock_once"
    // call, which checks this endpoint is called once.
    server
        .mock_room_messages()
        .from("prev-batch")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("oh well").event_id(event_id!("$1"))]))
        .mock_once()
        .mount()
        .await;

    // We run back-pagination with success.
    room_event_cache.pagination().run_backwards(20, once).await.unwrap();

    // And we get the back-paginated event.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );
    assert_eq!(diffs.len(), 1);

    assert_let!(VectorDiff::Insert { index: 0, value: event } = &diffs[0]);
    assert_event_matches_msg(event, "oh well");

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
    let room = server.sync_joined_room(&client, room_id).await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    // The first back-pagination will return these two events.
    //
    // Note: it's important to return the same event that came from sync: since we
    // will back-paginate without a prev-batch token first, we'll back-paginate
    // from the end of the timeline, which must include the event we got from
    // sync.

    // We need to trigger the following conditions:
    // - a back-pagination starts,
    // - but then we get events from sync, before the back-pagination is done.
    //
    // The following things will happen:
    // - We don't have a prev-batch token to start with, so the first
    //   back-pagination doesn't start
    // before DEFAULT_WAIT_FOR_TOKEN_DURATION seconds.
    // - While the back-pagination is actually running, we need a sync adding events
    //   to happen
    // (after DEFAULT_WAIT_FOR_TOKEN_DURATION + 500 milliseconds).
    // - The back-pagination finishes after this sync (after
    //   DEFAULT_WAIT_FOR_TOKEN_DURATION + 1
    // second).

    let wait_time = Duration::from_millis(500);
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default()
            .end_token("prev_batch")
            .events(vec![
                f.text_msg("world").event_id(event_id!("$2")).into_raw_timeline(),
                f.text_msg("hello").event_id(event_id!("$3")).into_raw_timeline(),
            ])
            .delayed(2 * wait_time))
        .mock_once()
        .mount()
        .await;

    // The second round of back-pagination will return this one.
    server
        .mock_room_messages()
        .from("prev_batch")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("oh well").event_id(event_id!("$1"))]))
        .mock_once()
        .mount()
        .await;

    let pagination = room_event_cache.pagination();

    // Run pagination: since there's no token, we'll wait a bit for a sync to return
    // one, and since there's none, we'll end up starting from the end of the
    // timeline.
    let pagination_clone = pagination.clone();

    let first_pagination = spawn(async move { pagination_clone.run_backwards(20, once).await });

    // Make sure we've waited for the initial token long enough (3 seconds, as of
    // 2024-12-16).
    sleep(Duration::from_millis(3000) + wait_time).await;
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").event_id(event_id!("$3"))),
        )
        .await;

    first_pagination.await.expect("joining must work").expect("first backpagination must work");

    // Second pagination will be instant.
    pagination.run_backwards(20, once).await.unwrap();

    // The linked chunk should contain the events in the correct order.
    let (events, _stream) = room_event_cache.subscribe().await.unwrap();

    assert_eq!(events.len(), 3, "{events:?}");
    assert_event_matches_msg(&events[0], "oh well");
    assert_event_matches_msg(&events[1], "hello");
    assert_event_matches_msg(&events[2], "world");
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
        .ok(RoomMessagesResponseTemplate::default().end_token("prev_batch"))
        .mock_once()
        .mount()
        .await;

    // The second round of back-pagination will return this one.
    server
        .mock_room_messages()
        .from("prev_batch")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("hello").event_id(event_id!("$1"))]))
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

#[async_test]
async fn test_no_gap_stored_after_deduplicated_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();
    event_cache.enable_storage().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    let initial_events = vec![
        f.text_msg("hello").event_id(event_id!("$1")).into_raw_sync(),
        f.text_msg("world").event_id(event_id!("$2")).into_raw_sync(),
        f.text_msg("sup").event_id(event_id!("$3")).into_raw_sync(),
    ];

    // Start with a room with a few events, limited timeline and prev-batch token.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_bulk(initial_events.clone())
                .set_timeline_limited()
                .set_timeline_prev_batch("prev-batch".to_owned()),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (events, mut stream) = room_event_cache.subscribe().await.unwrap();

    if events.is_empty() {
        assert_let_timeout!(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = stream.recv());
    }

    drop(events);

    // Backpagination will return nothing.
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default())
        .mock_once()
        .mount()
        .await;

    let pagination = room_event_cache.pagination();

    // Run pagination once: it will consume the unique gap we had.
    pagination.run_backwards(20, once).await.unwrap();

    // Now simulate that the sync returns the same events (which can happen with
    // simplified sliding sync).
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_bulk(initial_events)
                .set_timeline_limited()
                .set_timeline_prev_batch("prev-batch".to_owned()),
        )
        .await;

    assert!(stream.is_empty());

    // If this back-pagination fails, that's because we've stored a gap that's
    // useless. It should be short-circuited because there's no previous gap.
    let outcome = pagination.run_backwards(20, once).await.unwrap();
    assert!(outcome.reached_start);

    let (events, stream) = room_event_cache.subscribe().await.unwrap();
    assert_event_matches_msg(&events[0], "hello");
    assert_event_matches_msg(&events[1], "world");
    assert_event_matches_msg(&events[2], "sup");
    assert_eq!(events.len(), 3);

    assert!(stream.is_empty());
}

#[async_test]
async fn test_no_gap_stored_after_deduplicated_backpagination() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();
    event_cache.enable_storage().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    // Start with a room with a single event, limited timeline and prev-batch token.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("sup").event_id(event_id!("$3")).into_raw_sync())
                .set_timeline_limited()
                .set_timeline_prev_batch("prev-batch".to_owned()),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (events, mut stream) = room_event_cache.subscribe().await.unwrap();

    if events.is_empty() {
        assert_let_timeout!(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = stream.recv());
    }

    drop(events);

    // Now, simulate that we expanded the timeline window with sliding sync, by
    // returning more items.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_bulk(vec![
                    f.text_msg("hello").event_id(event_id!("$1")).into_raw_sync(),
                    f.text_msg("world").event_id(event_id!("$2")).into_raw_sync(),
                    f.text_msg("sup").event_id(event_id!("$3")).into_raw_sync(),
                ])
                .set_timeline_limited()
                .set_timeline_prev_batch("prev-batch2".to_owned()),
        )
        .await;

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = stream.recv()
    );
    assert_eq!(diffs.len(), 2);

    // `$ev3` is duplicated, the older `$ev3` event is removed
    assert_matches!(&diffs[0], VectorDiff::Remove { index } => {
        assert_eq!(*index, 0);
    });
    // `$ev1`, `$ev2` and `$ev3` are added.
    assert_matches!(&diffs[1], VectorDiff::Append { values: events } => {
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].event_id().unwrap().as_str(), "$1");
        assert_eq!(events[1].event_id().unwrap().as_str(), "$2");
        assert_eq!(events[2].event_id().unwrap().as_str(), "$3");
    });

    assert!(stream.is_empty());

    // For prev-batch2, the back-pagination returns nothing.
    server
        .mock_room_messages()
        .from("prev-batch2")
        .ok(RoomMessagesResponseTemplate::default())
        .mock_once()
        .mount()
        .await;

    // For prev-batch, the back-pagination returns two events we already know, and a
    // previous batch token.
    server
        .mock_room_messages()
        .from("prev-batch")
        .ok(RoomMessagesResponseTemplate::default().end_token("prev-batch3").events(vec![
            // Items in reverse order, since this is back-pagination.
            f.text_msg("world").event_id(event_id!("$2")).into_raw_timeline(),
            f.text_msg("hello").event_id(event_id!("$1")).into_raw_timeline(),
        ]))
        .mock_once()
        .mount()
        .await;

    let pagination = room_event_cache.pagination();

    // Run pagination once: it will consume prev-batch2 first, which is the most
    // recent token.
    let outcome = pagination.run_backwards(20, once).await.unwrap();

    // The pagination is empty: no new event.
    assert!(outcome.reached_start);
    assert!(outcome.events.is_empty());
    assert!(stream.is_empty());

    // Run pagination a second time: it will consume prev-batch, which is the least
    // recent token.
    let outcome = pagination.run_backwards(20, once).await.unwrap();

    // The pagination contains events, but they are all duplicated; the gap is
    // replaced by zero event: nothing happens.
    assert!(outcome.reached_start.not());
    assert_eq!(outcome.events.len(), 2);
    assert!(stream.is_empty());

    // If this back-pagination fails, that's because we've stored a gap that's
    // useless. It should be short-circuited because storing the previous gap was
    // useless.
    let outcome = pagination.run_backwards(20, once).await.unwrap();
    assert!(outcome.reached_start);
    assert!(outcome.events.is_empty());
    assert!(stream.is_empty());

    let (events, stream) = room_event_cache.subscribe().await.unwrap();
    assert_event_matches_msg(&events[0], "hello");
    assert_event_matches_msg(&events[1], "world");
    assert_event_matches_msg(&events[2], "sup");
    assert_eq!(events.len(), 3);

    assert!(stream.is_empty());
}

#[async_test]
async fn test_dont_delete_gap_that_wasnt_inserted() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();
    event_cache.enable_storage().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    // Start with a room with a single event, limited timeline and prev-batch token.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("sup").event_id(event_id!("$3")).into_raw_sync())
                .set_timeline_limited()
                .set_timeline_prev_batch("prev-batch".to_owned()),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (events, mut stream) = room_event_cache.subscribe().await.unwrap();
    if events.is_empty() {
        assert_let_timeout!(Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = stream.recv());
    }
    drop(events);

    // Back-paginate to consume the existing gap.
    // Say the back-pagination doesn't return anything.
    server
        .mock_room_messages()
        .from("prev-batch")
        .ok(RoomMessagesResponseTemplate::default())
        .mock_once()
        .mount()
        .await;
    room_event_cache.pagination().run_backwards(20, once).await.unwrap();

    // This doesn't cause an update, because nothing changed.
    assert!(stream.is_empty());

    // After a restart, a sync with the same sliding sync window may return the same
    // events, but no prev-batch token this time.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![f
                .text_msg("sup")
                .event_id(event_id!("$3"))
                .into_raw_sync()]),
        )
        .await;

    // This doesn't cause an update, because nothing changed.
    assert!(stream.is_empty());
}

#[async_test]
async fn test_apply_redaction_when_redaction_comes_later() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();
    event_cache.enable_storage().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    // Start with a room with two events.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("inapprops").event_id(event_id!("$1")).into_raw_sync(),
            ),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    // Wait for the first event.
    let (events, mut subscriber) = room_event_cache.subscribe().await.unwrap();
    if events.is_empty() {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = subscriber.recv()
        );
    }

    // Sync a redaction for the event $1.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.redaction(event_id!("$1")).into_raw_sync()),
        )
        .await;

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );

    assert_eq!(diffs.len(), 2);

    // First, the redaction event itself.
    {
        assert_let!(VectorDiff::Append { values: new_events } = &diffs[0]);
        assert_eq!(new_events.len(), 1);
        let ev = new_events[0].raw().deserialize().unwrap();
        assert_let!(
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(ev)) = ev
        );
        assert_eq!(ev.redacts(&RoomVersionId::V1).unwrap(), event_id!("$1"));
    }

    // Then, we have an update for the redacted event.
    {
        assert_let!(VectorDiff::Set { index: 0, value: redacted_event } = &diffs[1]);
        let ev = redacted_event.raw().deserialize().unwrap();
        assert_let!(
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(ev)) = ev
        );
        // The event has been redacted!
        assert_matches!(ev.as_original(), None);
    }

    // And done for now.
    assert!(subscriber.is_empty());
}

#[async_test]
async fn test_apply_redaction_when_redacted_and_redaction_are_in_same_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();
    event_cache.enable_storage().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let room = server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (_events, mut subscriber) = room_event_cache.subscribe().await.unwrap();

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    // Now include a sync with both the original event *and* the redacted one.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("bleh").event_id(event_id!("$2")).into_raw_sync())
                .add_timeline_event(f.redaction(event_id!("$2")).into_raw_sync()),
        )
        .await;

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );

    assert_eq!(diffs.len(), 2);

    // First, we get an update with all the new events.
    {
        assert_let!(VectorDiff::Append { values: new_events } = &diffs[0]);
        assert_eq!(new_events.len(), 2);

        // The original event.
        let ev = new_events[0].raw().deserialize().unwrap();
        assert_let!(
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(ev)) = ev
        );
        assert_eq!(ev.as_original().unwrap().content.body(), "bleh");

        // The redaction.
        let ev = new_events[1].raw().deserialize().unwrap();
        assert_let!(
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(ev)) = ev
        );
        assert_eq!(ev.redacts(&RoomVersionId::V1).unwrap(), event_id!("$2"));
    }

    // Then the redaction of the event happens separately.
    {
        assert_let!(VectorDiff::Set { index: 0, value: redacted_event } = &diffs[1]);
        let ev = redacted_event.raw().deserialize().unwrap();
        assert_let!(
            AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(ev)) = ev
        );
        // The event has been redacted!
        assert_matches!(ev.as_original(), None);
    }

    // That's all, folks!
    assert!(subscriber.is_empty());
}
