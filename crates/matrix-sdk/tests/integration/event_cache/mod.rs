use std::{ops::Not, sync::Arc, time::Duration};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::FutureExt;
use imbl::Vector;
use matrix_sdk::{
    assert_let_timeout, assert_next_matches_with_timeout,
    deserialized_responses::TimelineEvent,
    event_cache::{
        BackPaginationOutcome, EventCacheError, RoomEventCacheUpdate, RoomPaginationStatus,
    },
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
    store::StoreConfig,
    test_utils::{
        assert_event_matches_msg,
        mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    },
};
use matrix_sdk_base::event_cache::{
    Gap,
    store::{EventCacheStore, MemoryStore},
};
use matrix_sdk_test::{ALICE, BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{
    EventId, event_id,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, TimelineEventType,
        room::message::RoomMessageEventContentWithoutRelation,
    },
    room_id,
    room_version_rules::RedactionRules,
    user_id,
};
use tokio::{spawn, sync::broadcast, time::sleep};

mod threads;

macro_rules! assert_event_id {
    ($timeline_event:expr, $event_id:literal) => {
        assert_eq!($timeline_event.event_id().unwrap().as_str(), $event_id);
    };
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
    let (events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

    // Then at first it contains the two initial events.
    assert_eq!(events.len(), 2);
    assert_event_matches_msg(&events[0], "hey there");
    assert_event_matches_msg(&events[1], "hoy!");

    // `dexter` is ignored.
    server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_global_account_data(f.ignored_user_list([dexter.to_owned()]));
        })
        .await;

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

    // We do receive a clear.
    {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Clear = &diffs[0]);
    }

    // We do receive the new event.
    {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
        );
        assert_eq!(diffs.len(), 1);

        assert_let!(VectorDiff::Append { values: events } = &diffs[0]);
        assert_eq!(events.len(), 1);
        assert_event_matches_msg(&events[0], "i don't like this dexter");
    }

    // The other room has been cleared too.
    {
        let room = client.get_room(other_room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        let events = room_event_cache.events().await.unwrap();
        assert!(events.is_empty());
    }

    // That's all, folks!
    assert!(room_stream.is_empty());
}

/// Small helper for backpagination tests, to wait for things to stabilize.
async fn wait_for_initial_events(
    events: Vec<TimelineEvent>,
    room_stream: &mut broadcast::Receiver<RoomEventCacheUpdate>,
) {
    if events.is_empty() {
        // Wait for a first update.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = room_stream.recv()
        );

        // Read as much updates immediately available as possible.
        while let Some(Ok(_)) = room_stream.recv().now_or_never() {}
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
            .match_from("prev_batch")
            .ok(RoomMessagesResponseTemplate::default().events(vec![
                f.text_msg("world").event_id(event_id!("$2")),
                f.text_msg("hello").event_id(event_id!("$3")),
            ]))
            .mock_once()
            .mount()
            .await;

        // Then if I backpaginate,
        room_event_cache.pagination().run_backwards_once(20).await.unwrap()
    };

    // I'll get all the previous events, in "reverse" order (same as the response).
    let BackPaginationOutcome { events, reached_start } = outcome;

    // The event cache figures this is the last chunk of events in the room, because
    // there's no prior gap this time.
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

    // Another back-pagination doesn't return any new information.
    let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
    assert!(outcome.events.is_empty());
    assert!(outcome.reached_start);
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
    let mut global_events = Vec::new();

    // The first back-pagination will return these two.
    server
        .mock_room_messages()
        .match_from("prev_batch")
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
        .match_from("prev_batch2")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("oh well").event_id(event_id!("$4"))]))
        .mock_once()
        .mount()
        .await;

    // Then if I backpaginate in a loop,
    let pagination = room_event_cache.pagination();
    loop {
        let outcome = pagination.run_backwards_once(20).await.unwrap();
        global_events.extend(outcome.events);
        num_iterations += 1;
        if outcome.reached_start {
            break;
        }
    }

    // I'll get all the previous events, in two iterations, one for each
    // back-pagination.
    assert_eq!(num_iterations, 2);

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
    let mut global_events = Vec::new();

    // The first back-pagination will return these two.
    server
        .mock_room_messages()
        .match_from("prev_batch")
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
        .match_from("prev_batch2")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("oh well").event_id(event_id!("$4"))]))
        .mock_once()
        .mount()
        .await;

    // Then if I backpaginate in a loop,
    let pagination = room_event_cache.pagination();
    loop {
        let outcome = pagination.run_backwards_until(20).await.unwrap();
        global_events.extend(outcome.events);
        num_iterations += 1;
        if outcome.reached_start {
            break;
        }
    }

    // I'll get all the previous events,
    assert_eq!(num_iterations, 1); // in one iteration

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

    wait_for_initial_events(events, &mut room_stream).await;

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
        .match_from("first_backpagination")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("lalala").into_raw_timeline()])
            .with_delay(Duration::from_millis(500)))
        .mock_once()
        .mount()
        .await;

    // Mock the second back-pagination request, that will be hit after the reset
    // caused by the sync.
    server
        .mock_room_messages()
        .match_from("second_backpagination")
        .ok(RoomMessagesResponseTemplate::default()
            .end_token("third_backpagination")
            .events(vec![f.text_msg("finally!").into_raw_timeline()]))
        .mock_once()
        .mount()
        .await;

    // Run the pagination!
    let backpagination = spawn({
        let pagination = room_event_cache.pagination();
        async move { pagination.run_backwards_once(20).await }
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

    // Assert the updates as diffs.
    {
        // The room shrinks the linked chunk to the last event chunk, so it clears and
        // re-adds the latest event.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
        );
        assert_eq!(diffs.len(), 2);
        assert_matches!(&diffs[0], VectorDiff::Clear);
        assert_matches!(&diffs[1], VectorDiff::Append { values } => {
            assert_eq!(values.len(), 1);
            assert_event_matches_msg(&values[0], "heyo");
        });
    }

    {
        // Then we receive the event from the restarted back-pagination with
        // `second_backpagination`.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Insert { index, value: event } => {
            assert_eq!(*index, 0);
            assert_event_matches_msg(event, "finally!");
        });
    }

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

    // If we try to back-paginate with a token, it will hit the end of the timeline
    // and give us the resulting event.
    let BackPaginationOutcome { events, reached_start } =
        pagination.run_backwards_once(20).await.unwrap();

    assert!(reached_start);

    // And we get notified about the new event.
    assert_eq!(events.len(), 1);
    assert_event_matches_msg(&events[0], "hi");

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

    // At the beginning, the paginator is in the idle state.
    let pagination = room_event_cache.pagination();
    let mut pagination_status = pagination.status();
    assert_eq!(pagination_status.get(), RoomPaginationStatus::Idle { hit_timeline_start: false });

    // If we try to back-paginate with a token, it will hit the end of the timeline
    // and give us the resulting event.
    let BackPaginationOutcome { events, reached_start } =
        pagination.run_backwards_once(20).await.unwrap();

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

    // And the paginator state delivers this as an update, and is internally
    // consistent with it:
    assert_next_matches_with_timeout!(
        pagination_status,
        RoomPaginationStatus::Idle { hit_timeline_start: true }
    );

    // When a limited sync comes back from the server,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .set_timeline_limited()
                .set_timeline_prev_batch("prev-batch".to_owned()),
        )
        .await;

    // We have a limited sync, which triggers a shrink to the latest chunk: a gap.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );
    assert_eq!(diffs.len(), 1);
    assert_matches!(&diffs[0], VectorDiff::Clear);

    // The paginator state is reset: status set to Idle, hasn't hit the timeline
    // start.
    assert_next_matches_with_timeout!(
        pagination_status,
        RoomPaginationStatus::Idle { hit_timeline_start: false }
    );

    assert!(room_stream.is_empty());
}

#[async_test]
async fn test_limited_timeline_with_storage() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Don't forget to subscribe and like^W enable storage!
    event_cache.subscribe().unwrap();

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
    //   back-pagination doesn't start before DEFAULT_WAIT_FOR_TOKEN_DURATION
    //   seconds.
    // - While the back-pagination is actually running, we need a sync adding events
    //   to happen (after DEFAULT_WAIT_FOR_TOKEN_DURATION + 500 milliseconds).
    // - The back-pagination finishes after this sync (after
    //   DEFAULT_WAIT_FOR_TOKEN_DURATION + 1 seconds).

    let wait_time = Duration::from_millis(500);
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default()
            .end_token("prev_batch")
            .events(vec![
                f.text_msg("world").event_id(event_id!("$3")).into_raw_timeline(),
                f.text_msg("hello").event_id(event_id!("$2")).into_raw_timeline(),
            ])
            .with_delay(2 * wait_time))
        .mock_once()
        .mount()
        .await;

    // The second round of back-pagination will return this one.
    server
        .mock_room_messages()
        .match_from("prev_batch")
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

    let first_pagination = spawn(async move { pagination_clone.run_backwards_once(20).await });

    // Make sure we've waited for the initial token long enough (3 seconds, as of
    // 2024-12-16).
    sleep(Duration::from_millis(3000) + wait_time).await;
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("world").event_id(event_id!("$3"))),
        )
        .await;

    first_pagination.await.expect("joining must work").expect("first backpagination must work");

    // Second pagination will be instant.
    pagination.run_backwards_once(20).await.unwrap();

    // The linked chunk should contain the events in the correct order.
    let events = room_event_cache.events().await.unwrap();

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
        .match_from("prev_batch")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("hello").event_id(event_id!("$1"))]))
        .mock_once()
        .mount()
        .await;

    let pagination = room_event_cache.pagination();

    // Run pagination twice.
    pagination.run_backwards_once(20).await.unwrap();
    pagination.run_backwards_once(20).await.unwrap();

    // The linked chunk should contain the events in the correct order.
    let events = room_event_cache.events().await.unwrap();

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

    let pagination = room_event_cache.pagination();

    // Backpagination will return nothing.
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default())
        .mock_once()
        .mount()
        .await;

    // The first sync was limited, so we have unloaded the full linked chunk, and it
    // only contains the events returned by the sync.
    //
    // The first back-pagination will hit the network, and let us know we've reached
    // the end of the room.

    let outcome = pagination.run_backwards_once(20).await.unwrap();
    assert!(outcome.reached_start);
    assert!(outcome.events.is_empty());

    // Now simulate that the sync returns the same events (which can happen with
    // simplified sliding sync), also as a limited sync.
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

    {
        let events = room_event_cache.events().await.unwrap();
        assert_event_matches_msg(&events[0], "hello");
        assert_event_matches_msg(&events[1], "world");
        assert_event_matches_msg(&events[2], "sup");
        assert_eq!(events.len(), 3);
    }

    // If any of the following back-paginations fail with a network error, that's
    // because we've stored a gap that's useless. All back-paginations must be
    // loading from the store.
    //
    // The sync was limited, which unloaded the linked chunk, and reloaded only the
    // final events chunk.

    let outcome = pagination.run_backwards_once(20).await.unwrap();
    assert!(outcome.events.is_empty());
    assert!(outcome.reached_start);

    {
        let events = room_event_cache.events().await.unwrap();
        assert_event_matches_msg(&events[0], "hello");
        assert_event_matches_msg(&events[1], "world");
        assert_event_matches_msg(&events[2], "sup");
        assert_eq!(events.len(), 3);
    }
}

#[async_test]
async fn test_no_gap_stored_after_deduplicated_backpagination() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

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

    assert_matches!(&diffs[0], VectorDiff::Clear);

    // Then the latest event chunk is reloaded.
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
        .match_from("prev-batch2")
        .ok(RoomMessagesResponseTemplate::default())
        .mock_once()
        .mount()
        .await;

    // Run pagination once: it will consume prev-batch2 first, which is the most
    // recent token, which returns an empty batch, thus indicating the start of the
    // room; but we still have a chunk in storage, so it appears like it's not the
    // start *yet*.
    let pagination = room_event_cache.pagination();

    let outcome = pagination.run_backwards_once(20).await.unwrap();
    assert!(outcome.reached_start.not());
    assert!(outcome.events.is_empty());
    assert!(stream.is_empty());

    // For prev-batch, the back-pagination returns two events we already know, and a
    // previous batch token.
    server
        .mock_room_messages()
        .match_from("prev-batch")
        .ok(RoomMessagesResponseTemplate::default().end_token("prev-batch3").events(vec![
            // Items in reverse order, since this is back-pagination.
            f.text_msg("world").event_id(event_id!("$2")).into_raw_timeline(),
            f.text_msg("hello").event_id(event_id!("$1")).into_raw_timeline(),
        ]))
        .mock_once()
        .mount()
        .await;

    // Run pagination a second time: it will consume prev-batch, which is the least
    // recent token.
    let outcome = pagination.run_backwards_once(20).await.unwrap();
    assert!(outcome.reached_start);
    assert!(outcome.events.is_empty());
    assert!(stream.is_empty());

    // If this back-pagination fails, that's because it's trying to hit network. In
    // that case, it means we stored the gap with the prev-batch3 token, while
    // we shouldn't have to, since it is useless; all events were deduplicated
    // from the previous pagination.

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
        .match_from("prev-batch")
        .ok(RoomMessagesResponseTemplate::default())
        .mock_once()
        .mount()
        .await;
    room_event_cache.pagination().run_backwards_once(20).await.unwrap();

    // This doesn't cause an update, because nothing changed.
    assert!(stream.is_empty());

    // After a restart, a sync with the same sliding sync window may return the same
    // events, but no prev-batch token this time.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                f.text_msg("sup").event_id(event_id!("$3")).into_raw_sync(),
            ]),
        )
        .await;

    // This doesn't cause an update, because nothing changed.
    assert!(stream.is_empty());
}

#[async_test]
async fn test_apply_redaction_when_redaction_comes_later() {
    let server = MatrixMockServer::new().await;

    // Create a manual event cache store, so we can reuse it across multiple
    // clients.
    let state_memory_store = matrix_sdk_base::store::MemoryStore::new();
    let event_cache_store = Arc::new(MemoryStore::new());
    let store_config = StoreConfig::new("hodlor".to_owned())
        .state_store(state_memory_store)
        .event_cache_store(event_cache_store);
    let client = server
        .client_builder()
        .on_builder(|builder| builder.store_config(store_config.clone()))
        .build()
        .await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(user_id!("@a:b.c"));

    // Start with a room with one event.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("inapprops").event_id(event_id!("$1")).into_raw_sync(),
            ),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    // Wait for the event.
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
        assert_eq!(ev.redacts(&RedactionRules::V1).unwrap(), event_id!("$1"));
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

    // If another client is created with the same store, then the stored event is
    // already redacted.
    drop(client);

    let client = server
        .client_builder()
        .on_builder(|builder| builder.store_config(store_config))
        .build()
        .await;
    client.event_cache().subscribe().unwrap();
    let room = client.get_room(room_id).unwrap();
    let (cache, _drop_handles) = room.event_cache().await.unwrap();

    let events = cache.events().await.unwrap();

    // We have two events:
    assert_eq!(events.len(), 2);

    // The initial event (that's been redacted),
    let ev = events[0].raw().cast_ref_unchecked::<AnySyncMessageLikeEvent>().deserialize().unwrap();
    assert!(ev.is_redacted());

    // And the redacted event.
    assert_eq!(
        events[1].raw().deserialize().unwrap().event_type(),
        TimelineEventType::RoomRedaction
    );
}

#[async_test]
async fn test_apply_redaction_on_an_in_store_event() {
    let room_id = room_id!("!foo:bar.baz");
    let event_factory = EventFactory::new().room(room_id).sender(&ALICE);

    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    // Set up the event cache store.
    {
        let event_cache_store = client.event_cache_store().lock().await.unwrap();

        // The event cache contains 2 chunks as such (from older to newewst):
        // 1. a chunk of 1 item, the one we are going to redact!
        // 2. a chunk of 1 item, the chunk that is going to be loaded.
        event_cache_store
            .as_clean()
            .unwrap()
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    // chunk #1
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    // … and its item
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![
                            event_factory.text_msg("foo").event_id(event_id!("$ev0")).into_event(),
                        ],
                    },
                    // chunk #2
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    // … and its item
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![
                            event_factory.text_msg("foo").event_id(event_id!("$ev1")).into_event(),
                        ],
                    },
                ],
            )
            .await
            .unwrap();
    }

    // Set up the event cache.
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room = mock_server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _room_event_cache_drop_handle) = room.event_cache().await.unwrap();

    let (initial_updates, mut updates_stream) = room_event_cache.subscribe().await.unwrap();

    // Initial events!
    //
    // Only 1 events is loaded, as expected, from the last chunk.
    {
        assert_eq!(initial_updates.len(), 1);
        assert_event_id!(initial_updates[0], "$ev1");

        // The stream of updates is waiting, patiently.
        assert!(updates_stream.is_empty());
    }

    // Sync a redaction for `$ev0`.
    mock_server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                event_factory
                    .redaction(event_id!("$ev0"))
                    .event_id(event_id!("$ev2"))
                    .into_raw_sync(),
            ),
        )
        .await;

    // Let's check the stream.
    let update = updates_stream.recv().await.unwrap();

    assert_matches!(update, RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
        // 1 diff for the `m.room.redaction`. The event being redacted is not
        // in-memory yet, it's only in the store, so no update for it.
        assert_eq!(diffs.len(), 1);

        assert_matches!(&diffs[0], VectorDiff::Append { values: events } => {
            assert_eq!(events.len(), 1);
            assert_event_id!(&events[0], "$ev2");
        });
    });

    assert!(updates_stream.is_empty());

    // To see if `$ev0` has been redacted in the store, let's paginate!
    let outcome = room_event_cache.pagination().run_backwards_until(1).await.unwrap();

    // 1 event, no surprise.
    assert_eq!(outcome.events.len(), 1);
    assert_event_id!(outcome.events[0], "$ev0");
    assert_matches!(
        outcome.events[0].raw().deserialize().unwrap(),
        AnySyncTimelineEvent::MessageLike(event) => {
            // The event has been redacted!
            assert!(event.is_redacted());
        }
    );

    // Let's check the stream. It should reflect what the `pagination_outcome`
    // provides.
    let update = updates_stream.recv().await.unwrap();

    assert_matches!(update, RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
        assert_eq!(diffs.len(), 1);

        assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
            assert_event_id!(event, "$ev0");
            assert_matches!(
                event.raw().deserialize().unwrap(),
                AnySyncTimelineEvent::MessageLike(event) => {
                    // The event has been redacted!
                    assert!(event.is_redacted());
                }
            );
        });
    });

    assert!(updates_stream.is_empty());
}

#[async_test]
async fn test_apply_redaction_when_redacted_and_redaction_are_in_same_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let event_cache = client.event_cache();

    // Immediately subscribe the event cache to sync updates.
    event_cache.subscribe().unwrap();

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
        assert_eq!(ev.redacts(&RedactionRules::V1).unwrap(), event_id!("$2"));
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

#[async_test]
async fn test_lazy_loading() {
    let room_id = room_id!("!foo:bar.baz");
    let event_factory = EventFactory::new().room(room_id).sender(&ALICE);

    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    // Set up the event cache store.
    {
        let event_cache_store = client.event_cache_store().lock().await.unwrap();

        // The event cache contains 4 chunks as such (from newest to older):
        // 4. a chunk of 7 items
        // 3. a chunk of 5 items
        // 2. a chunk of a gap
        // 1. a chunk of 6 items
        event_cache_store
            .as_clean()
            .unwrap()
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    // chunk #1
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    // … and its 6 items
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: (0..6)
                            .map(|nth| {
                                event_factory
                                    .text_msg("foo")
                                    .event_id(&EventId::parse(format!("$ev0_{nth}")).unwrap())
                                    .into_event()
                            })
                            .collect::<Vec<_>>(),
                    },
                    // chunk #2
                    Update::NewGapChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                        gap: Gap { prev_token: "raclette".to_owned() },
                    },
                    // chunk #3
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(1)),
                        new: ChunkIdentifier::new(2),
                        next: None,
                    },
                    // … and its 5 items
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(2), 0),
                        items: (0..5)
                            .map(|nth| {
                                event_factory
                                    .text_msg("foo")
                                    .event_id(&EventId::parse(format!("$ev2_{nth}")).unwrap())
                                    .into_event()
                            })
                            .collect::<Vec<_>>(),
                    },
                    // chunk #4
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(2)),
                        new: ChunkIdentifier::new(3),
                        next: None,
                    },
                    // … and its 7 items
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(3), 0),
                        items: (0..7)
                            .map(|nth| {
                                event_factory
                                    .text_msg("foo")
                                    .event_id(&EventId::parse(format!("$ev3_{nth}")).unwrap())
                                    .into_event()
                            })
                            .collect::<Vec<_>>(),
                    },
                ],
            )
            .await
            .unwrap();
    }

    // Set up the event cache.
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room = mock_server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _room_event_cache_drop_handle) = room.event_cache().await.unwrap();

    let (initial_updates, mut updates_stream) = room_event_cache.subscribe().await.unwrap();

    // Initial events!
    //
    // Only 7 events are loaded! They are from the last chunk.
    {
        assert_eq!(initial_updates.len(), 7);

        // Yummy, exactly what we expect!
        assert_event_id!(initial_updates[0], "$ev3_0");
        assert_event_id!(initial_updates[1], "$ev3_1");
        assert_event_id!(initial_updates[2], "$ev3_2");
        assert_event_id!(initial_updates[3], "$ev3_3");
        assert_event_id!(initial_updates[4], "$ev3_4");
        assert_event_id!(initial_updates[5], "$ev3_5");
        assert_event_id!(initial_updates[6], "$ev3_6");

        // The stream of updates is waiting, patiently.
        assert!(updates_stream.is_empty());
    }

    // Let's paginate to load more!
    //
    // One more chunk will be loaded from the store. This new chunk contains 5
    // items. No need to reach the network.
    {
        let outcome = room_event_cache.pagination().run_backwards_until(1).await.unwrap();

        // Oh! 5 events! How classy.
        assert_eq!(outcome.events.len(), 5);

        // Hello you. Well… Uoy olleh! Remember, this is a backwards pagination, so
        // events are returned in reverse order.
        assert_event_id!(outcome.events[0], "$ev2_4");
        assert_event_id!(outcome.events[1], "$ev2_3");
        assert_event_id!(outcome.events[2], "$ev2_2");
        assert_event_id!(outcome.events[3], "$ev2_1");
        assert_event_id!(outcome.events[4], "$ev2_0");

        // And there is more, but this, kids, is for later.
        assert!(outcome.reached_start.not());

        // Let's check the stream. It should reflect what the
        // `pagination_outcome` provides.
        let update = updates_stream.recv().await.unwrap();

        assert_matches!(update, RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
            // 5 diffs?! *feigns surprise*
            assert_eq!(diffs.len(), 5);

            // Hello you. Again. This time in the expected order because it
            // reflects the order in the linked chunk.
            assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
                assert_event_id!(event, "$ev2_0");
            });
            assert_matches!(&diffs[1], VectorDiff::Insert { index: 1, value: event } => {
                assert_event_id!(event, "$ev2_1");
            });
            assert_matches!(&diffs[2], VectorDiff::Insert { index: 2, value: event } => {
                assert_event_id!(event, "$ev2_2");
            });
            assert_matches!(&diffs[3], VectorDiff::Insert { index: 3, value: event } => {
                assert_event_id!(event, "$ev2_3");
            });
            assert_matches!(&diffs[4], VectorDiff::Insert { index: 4, value: event } => {
                assert_event_id!(event, "$ev2_4");
            });
        });

        assert!(updates_stream.is_empty());
    }

    // This is a funny game. We are having fun, don't we? Let's paginate to load
    // moaare!
    //
    // One more chunk will be loaded from the store. This new chunk contains a
    // gap. Network will be reached. 4 events will be received, and inserted in the
    // event cache store, forever 🫶.
    {
        let _network_pagination = mock_server
            .mock_room_messages()
            .match_from("raclette")
            .ok(RoomMessagesResponseTemplate::default().end_token("numerobis").events(
                (1..5) // Yes, the 0nth will be fetched with the next pagination.
                    // Backwards pagination implies events are in “reverse order”.
                    .rev()
                    .map(|nth| {
                        event_factory
                            .text_msg("foo")
                            .event_id(&EventId::parse(format!("$ev1_{nth}")).unwrap())
                    })
                    .collect::<Vec<_>>(),
            ))
            .mock_once()
            .mount_as_scoped()
            .await;

        let outcome = room_event_cache.pagination().run_backwards_until(1).await.unwrap();

        // 🙈… 4 events! Of course. We've never doubt.
        assert_eq!(outcome.events.len(), 4);

        // Hello you, in reverse order because this is a backward pagination.
        assert_event_id!(outcome.events[0], "$ev1_4");
        assert_event_id!(outcome.events[1], "$ev1_3");
        assert_event_id!(outcome.events[2], "$ev1_2");
        assert_event_id!(outcome.events[3], "$ev1_1");

        // And there is more because we didn't reach the start of the timeline yet.
        assert!(outcome.reached_start.not());

        // Let's check the stream. It should reflect what the
        // `pagination_outcome` provides.
        let update = updates_stream.recv().await.unwrap();

        assert_matches!(update, RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
            // 1, … 2, … 3, … 4 diffs! Quod Erat Demonstrandum.
            assert_eq!(diffs.len(), 4);

            // Hello you. Again. And again, in the expected order.
            assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
                assert_event_id!(event, "$ev1_1");
            });
            assert_matches!(&diffs[1], VectorDiff::Insert { index: 1, value: event } => {
                assert_event_id!(event, "$ev1_2");
            });
            assert_matches!(&diffs[2], VectorDiff::Insert { index: 2, value: event } => {
                assert_event_id!(event, "$ev1_3");
            });
            assert_matches!(&diffs[3], VectorDiff::Insert { index: 3, value: event } => {
                assert_event_id!(event, "$ev1_4");
            });
        });

        assert!(updates_stream.is_empty());
    }

    // More interesting now. Let's paginate again!
    //
    // A new chunk containing a gap has been inserted by the previous network
    // pagination, because it contained an `end` token. Let's see how the
    // `EventCache` will reconcile all that. A new chunk will not be loaded from
    // the store because the last chunk is a gap.
    //
    // The new network pagination will return 1 new event, and 1 event already
    // known. The complex part is: how to detect if an event is known if not all
    // events are loaded in memory? This is what we mostly test here.
    {
        let _network_pagination = mock_server
            .mock_room_messages()
            .match_from("numerobis")
            .ok(RoomMessagesResponseTemplate::default().end_token("trois").events(vec![
                // Backwards pagination implies events are in “reverse order”.
                //
                // The new event.
                event_factory.text_msg("foo").event_id(event_id!("$ev1_0")),
                // And the event we already know in the event cache' store. It's not yet in memory,
                // but it is in the store for sure!
                event_factory.text_msg("foo").event_id(event_id!("$ev0_5")),
            ]))
            .mock_once()
            .mount_as_scoped()
            .await;

        let outcome = room_event_cache.pagination().run_backwards_until(1).await.unwrap();

        // 2 events; the already known is reinserted.
        assert_eq!(outcome.events.len(), 2);
        assert_event_id!(outcome.events[0], "$ev1_0");
        assert_event_id!(outcome.events[1], "$ev0_5");

        // Still not the start of the timeline.
        assert!(outcome.reached_start.not());

        // The stream is consistent with what we observed.
        let update = updates_stream.recv().await.unwrap();

        assert_matches!(update, RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
            // 2 diffs: one for inserting each event.
            //
            // We don't get a notification about the removal, because it happened *in the store*,
            // so it's not reflected in the vector that materializes the lazy-loaded chunk.
            assert_eq!(diffs.len(), 2);

            assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
                assert_event_id!(event, "$ev0_5");
            });
            assert_matches!(&diffs[1], VectorDiff::Insert { index: 1, value: event } => {
                assert_event_id!(event, "$ev1_0");
            });
        });

        assert!(updates_stream.is_empty());
    }

    // Continue to paginate.
    //
    // A new chunk containing a gap has been inserted by the previous network
    // pagination, because (i) it contained an `end` token, and (ii) not all the
    // fetched events were duplicated. Let's paginate again!
    //
    // The new network pagination will return, let's say, 2 known events. They will
    // all be deduplicated, so zero events will be inserted. However, we do paginate
    // until there's at least one event. The first pagination will return zero
    // event, the gap chunk will be removed, a second pagination will then run.
    // This time, the store will be hit.
    {
        let _network_pagination = mock_server
            .mock_room_messages()
            .match_from("trois")
            .ok(RoomMessagesResponseTemplate::default().end_token("quattuor").events(
                // Only events we already know about in the store.
                (4..6)
                    // Backwards pagination implies events are in “reverse order”.
                    .rev()
                    .map(|nth| {
                        event_factory
                            .text_msg("foo")
                            .event_id(&EventId::parse(format!("$ev0_{nth}")).unwrap())
                    })
                    .collect::<Vec<_>>(),
            ))
            .mock_once() // once!
            .mount_as_scoped()
            .await;

        let outcome = room_event_cache.pagination().run_backwards_until(1).await.unwrap();

        // 🙊 … 5 events! Wait, what? Yes! The network has returned 2 known events, they
        // have all been deduplicated, resulting in the removal of the gap chunk.
        // We then re-ran the pagination, and this time the store is reached, and it
        // reflects the deduplicated $ev0_5 from a previous back-pagination.
        assert_eq!(outcome.events.len(), 5);

        // Hello to all of you!
        assert_event_id!(outcome.events[0], "$ev0_4");
        assert_event_id!(outcome.events[1], "$ev0_3");
        assert_event_id!(outcome.events[2], "$ev0_2");
        assert_event_id!(outcome.events[3], "$ev0_1");
        assert_event_id!(outcome.events[4], "$ev0_0");

        // This was the start of the timeline \o/
        assert!(outcome.reached_start);

        // Let's check the stream for the last time.
        let update = updates_stream.recv().await.unwrap();

        assert_matches!(update, RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
            assert_eq!(diffs.len(), 5);

            assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
                assert_event_id!(event, "$ev0_0");
            });
            assert_matches!(&diffs[1], VectorDiff::Insert { index: 1, value: event } => {
                assert_event_id!(event, "$ev0_1");
            });
            assert_matches!(&diffs[2], VectorDiff::Insert { index: 2, value: event } => {
                assert_event_id!(event, "$ev0_2");
            });
            assert_matches!(&diffs[3], VectorDiff::Insert { index: 3, value: event } => {
                assert_event_id!(event, "$ev0_3");
            });
            assert_matches!(&diffs[4], VectorDiff::Insert { index: 4, value: event } => {
                assert_event_id!(event, "$ev0_4");
            });
        });
    }

    assert!(updates_stream.is_empty());
}

#[async_test]
async fn test_deduplication() {
    let room_id = room_id!("!foo:bar.baz");
    let event_factory = EventFactory::new().room(room_id).sender(&ALICE);

    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    // Set up the event cache store.
    {
        let event_cache_store = client.event_cache_store().lock().await.unwrap();

        // The event cache contains 2 chunks as such (from newest to older):
        // 2. a chunk of 4 items
        // 1. a chunk of 3 items
        event_cache_store
            .as_clean()
            .unwrap()
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    // chunk #0
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    // … and its 4 items
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: (0..4)
                            .map(|nth| {
                                event_factory
                                    .text_msg("foo")
                                    .event_id(&EventId::parse(format!("$ev0_{nth}")).unwrap())
                                    .into_event()
                            })
                            .collect::<Vec<_>>(),
                    },
                    // chunk #1
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    // … and its 3 items
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: (0..3)
                            .map(|nth| {
                                event_factory
                                    .text_msg("foo")
                                    .event_id(&EventId::parse(format!("$ev1_{nth}")).unwrap())
                                    .into_event()
                            })
                            .collect::<Vec<_>>(),
                    },
                ],
            )
            .await
            .unwrap();
    }

    // Set up the event cache.
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room = mock_server.sync_joined_room(&client, room_id).await;
    let (room_event_cache, _room_event_cache_drop_handle) = room.event_cache().await.unwrap();

    let (initial_updates, mut updates_stream) = room_event_cache.subscribe().await.unwrap();

    // One chunk has been loaded, so there are 3 events in memory.
    {
        assert_eq!(initial_updates.len(), 3);

        assert_event_id!(initial_updates[0], "$ev1_0");
        assert_event_id!(initial_updates[1], "$ev1_1");
        assert_event_id!(initial_updates[2], "$ev1_2");

        assert!(updates_stream.is_empty());
    }

    // Now let's imagine we have a sync.
    // Do you know what's funny? This sync is a bit weird. It's totally messy
    // but our system is robust, so nothing will fail.
    //
    // The sync contains 6 events:
    // - 2 of them are duplicated with events in the loaded chunk #1,
    // - 2 of them are duplicated with events in the store (so not loaded yet),
    // - 2 events are unique.
    mock_server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                // The 2 events duplicated with the ones from the loaded chunk.
                .add_timeline_event(event_factory.text_msg("foo").event_id(event_id!("$ev1_0")))
                .add_timeline_event(event_factory.text_msg("foo").event_id(event_id!("$ev1_2")))
                // The 2 events duplicated with the ones from the not-loaded chunk.
                .add_timeline_event(event_factory.text_msg("foo").event_id(event_id!("$ev0_1")))
                .add_timeline_event(event_factory.text_msg("foo").event_id(event_id!("$ev0_2")))
                // The 2 unique events.
                .add_timeline_event(event_factory.text_msg("foo").event_id(event_id!("$ev3_0")))
                .add_timeline_event(event_factory.text_msg("foo").event_id(event_id!("$ev3_1"))),
        )
        .await;

    // What should we see?
    // - On `updates_stream`: 2 events from the loaded chunk #1 must be removed, and
    //   6 events must be added inserted (!); indeed, 4 are removed and re-inserted
    //   at the back, plus 2 events are newly inserted at the back, so 6 are
    //   inserted,
    // - On the store, 2 events must be removed from chunk #0
    //
    // First off, let's check `updates_stream`.
    {
        let update = updates_stream.recv().await.unwrap();

        assert_matches!(update, RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
            // 3 diffs, of course.
            assert_eq!(diffs.len(), 3);

            // Note that index 2 is removed before index 0!
            assert_matches!(&diffs[0], VectorDiff::Remove { index } => {
                assert_eq!(*index, 2);
            });
            assert_matches!(&diffs[1], VectorDiff::Remove { index } => {
                assert_eq!(*index, 0);
            });
            assert_matches!(&diffs[2], VectorDiff::Append { values: events } => {
                assert_eq!(events.len(), 6);

                assert_event_id!(&events[0], "$ev1_0");
                assert_event_id!(&events[1], "$ev1_2");
                assert_event_id!(&events[2], "$ev0_1");
                assert_event_id!(&events[3], "$ev0_2");
                assert_event_id!(&events[4], "$ev3_0");
                assert_event_id!(&events[5], "$ev3_1");
            });
        });
    }

    // Hands in the air, don't touch your keyboard, let's see the state of the
    // store by paginating backwards. `$ev0_1` and `$ev0_2` **MUST be absent**.
    {
        let outcome = room_event_cache.pagination().run_backwards_until(1).await.unwrap();

        // Alrighty, we should get 2 events since 2 of 4 should have been removed.
        assert_eq!(outcome.events.len(), 2);

        // Hello, in reverse order because it's a backward pagination.
        assert_event_id!(outcome.events[0], "$ev0_3");
        assert_event_id!(outcome.events[1], "$ev0_0");

        // Let's check what the stream has to say.
        let update = updates_stream.recv().await.unwrap();

        assert_matches!(update, RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
            // 2 diffs, but who's counting?
            assert_eq!(diffs.len(), 2);

            assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
                assert_event_id!(event, "$ev0_0");
            });
            assert_matches!(&diffs[1], VectorDiff::Insert { index: 1, value: event } => {
                assert_event_id!(event, "$ev0_3");
            });
        });
    }
}

#[async_test]
async fn test_timeline_then_empty_timeline_then_deduplication_with_storage() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!galette:saucisse.bzh");
    let room = server.sync_joined_room(&client, room_id).await;

    let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

    // Previous batch of events which will be received via /messages, in
    // chronological order.
    let previous_events = [
        f.text_msg("previous1").event_id(event_id!("$prev1")).into_raw_timeline(),
        f.text_msg("previous2").event_id(event_id!("$prev2")).into_raw_timeline(),
        f.text_msg("previous3").event_id(event_id!("$prev3")).into_raw_timeline(),
    ];

    // Latest events which will be received via /sync, in chronological order.
    let latest_events = [
        f.text_msg("latest3").event_id(event_id!("$latest3")).into_raw_timeline(),
        f.text_msg("latest2").event_id(event_id!("$latest2")).into_raw_timeline(),
        f.text_msg("latest1").event_id(event_id!("$latest1")).into_raw_timeline(),
    ];

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (initial_events, mut subscriber) = room_event_cache.subscribe().await.unwrap();
    assert!(initial_events.is_empty());

    // Receive a sync with only the latest events.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .set_timeline_limited()
                .set_timeline_prev_batch("token-before-latest")
                .add_timeline_bulk(latest_events.clone().into_iter().map(ruma::serde::Raw::cast)),
        )
        .await;

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );
    assert_eq!(diffs.len(), 2);
    assert_let!(VectorDiff::Clear = &diffs[0]);
    assert_let!(VectorDiff::Append { values } = &diffs[1]);

    assert_eq!(values.len(), 3);
    assert_event_matches_msg(&values[0], "latest3");
    assert_event_matches_msg(&values[1], "latest2");
    assert_event_matches_msg(&values[2], "latest1");

    // Receive a timeline without any items.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .set_timeline_limited()
                .set_timeline_prev_batch("token-after-latest"),
        )
        .await;

    // The timeline is limited, so the linked chunk is shrunk to the latest chunk.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );
    assert_eq!(diffs.len(), 1);
    assert_let!(VectorDiff::Clear = &diffs[0]);

    assert!(subscriber.is_empty());

    // Back-paginate.
    let all_events = previous_events.into_iter().chain(latest_events).rev().collect::<Vec<_>>();

    server
        .mock_room_messages()
        // The prev_batch from the second sync.
        .match_from("token-after-latest")
        .ok(RoomMessagesResponseTemplate::default().end_token("messages-end-2").events(all_events))
        .named("messages-since-after-latest")
        .mount()
        .await;

    let outcome = room_event_cache.pagination().run_backwards_once(10).await.unwrap();
    assert!(outcome.reached_start.not());
    assert_eq!(outcome.events.len(), 6);

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );
    assert_eq!(diffs.len(), 1);

    // Yay.
    assert_let!(VectorDiff::Append { values } = &diffs[0]);
    assert_eq!(values.len(), 6);
    assert_event_matches_msg(&values[0], "previous1");
    assert_event_matches_msg(&values[1], "previous2");
    assert_event_matches_msg(&values[2], "previous3");
    assert_event_matches_msg(&values[3], "latest3");
    assert_event_matches_msg(&values[4], "latest2");
    assert_event_matches_msg(&values[5], "latest1");

    // That's all, folks!
    assert!(subscriber.is_empty());
}

#[async_test]
async fn test_dont_remove_only_gap() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!galette:saucisse.bzh");
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .set_timeline_limited()
                .set_timeline_prev_batch("brillat-savarin"),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    server
        .mock_room_messages()
        .match_from("brillat-savarin")
        .ok(RoomMessagesResponseTemplate::default())
        .named("room/messages")
        .mount()
        .await;

    // Back-paginate with the given token.
    let outcome = room_event_cache.pagination().run_backwards_once(16).await.unwrap();
    assert!(outcome.reached_start);
}

#[async_test]
async fn test_clear_all_rooms() {
    let sleeping_room_id = room_id!("!dodo:saucisse.bzh");
    let event_cache_store = Arc::new(MemoryStore::new());

    let f = EventFactory::new().room(sleeping_room_id);
    let ev0 = f.text_msg("hi").sender(*ALICE).event_id(event_id!("$ev0")).into_event();

    // Feed the cache with one room with one event, before the client is created.
    // This room will remain sleeping.
    {
        let cid = ChunkIdentifier::new(0);
        event_cache_store
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(sleeping_room_id),
                vec![
                    Update::NewItemsChunk { previous: None, new: cid, next: None },
                    Update::PushItems { at: Position::new(cid, 0), items: vec![ev0] },
                ],
            )
            .await
            .unwrap();
    }

    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.store_config(
                StoreConfig::new("hodlor".to_owned()).event_cache_store(event_cache_store.clone()),
            )
        })
        .build()
        .await;

    client.event_cache().subscribe().unwrap();

    // Another room gets a live event: it's loaded in the event cache now, while
    // sleeping_room_id is not.
    let room_id = room_id!("!galette:saucisse.bzh");
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("bonchourhan").sender(*BOB).event_id(event_id!("$ev1")),
            ),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (initial, mut room_updates) = room_event_cache.subscribe().await.unwrap();

    let mut initial = Vector::from(initial);
    // Wait for the ev1 event.
    if initial.is_empty() {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_updates.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_matches!(diffs[0], VectorDiff::Append { .. });
        diffs[0].clone().apply(&mut initial);
    }
    // The room state now contains one event.
    assert_eq!(initial.len(), 1);
    assert_event_id!(initial[0], "$ev1");

    // Now, clear all the rooms.
    client.event_cache().clear_all_rooms().await.unwrap();

    // We should get an update for the live room.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_updates.recv()
    );
    assert_eq!(diffs.len(), 1);
    assert_let!(VectorDiff::Clear = &diffs[0]);

    // The sleeping room should have been cleared too.
    let (maybe_last_chunk, _chunk_id_gen) =
        event_cache_store.load_last_chunk(LinkedChunkId::Room(sleeping_room_id)).await.unwrap();
    assert!(maybe_last_chunk.is_none());
}

#[async_test]
async fn test_sync_while_back_paginate() {
    let server = MatrixMockServer::new().await;

    let room_id = room_id!("!galette:saucisse.bzh");
    let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

    // Previous batch of events which will be received via /messages, in reverse
    // chronological order.
    let prev_events = vec![
        f.text_msg("messages3").event_id(event_id!("$messages3")).into_raw_timeline(),
        f.text_msg("messages2").event_id(event_id!("$messages2")).into_raw_timeline(),
        f.text_msg("messages1").event_id(event_id!("$messages1")).into_raw_timeline(),
    ];

    // Batch of events which will be received via /sync, in chronological
    // order.
    let sync_events = [
        f.text_msg("sync1").event_id(event_id!("$sync1")).into_raw_timeline(),
        f.text_msg("sync2").event_id(event_id!("$sync2")).into_raw_timeline(),
        f.text_msg("sync3").event_id(event_id!("$sync3")).into_raw_timeline(),
    ];

    let state_memory_store = matrix_sdk_base::store::MemoryStore::new();
    let store_config = StoreConfig::new("le_store".to_owned())
        .event_cache_store(Arc::new(MemoryStore::new()))
        .state_store(state_memory_store);

    {
        // First, initialize the sync so the client is aware of the room, in the state
        // store.
        let client = server
            .client_builder()
            .on_builder(|builder| builder.store_config(store_config.clone()))
            .build()
            .await;
        server.sync_joined_room(&client, room_id).await;
    }

    // Then, use a new client that will restore the state from the state store, and
    // with an empty event cache store.
    let client = server
        .client_builder()
        .on_builder(|builder| builder.store_config(store_config))
        .build()
        .await;
    let room = client.get_room(room_id).unwrap();

    client.event_cache().subscribe().unwrap();

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (initial_events, mut subscriber) = room_event_cache.subscribe().await.unwrap();
    assert!(initial_events.is_empty());

    // Mock /messages in case we use the prev_batch token from sync.
    server
        .mock_room_messages()
        .match_from("token-before-sync-from-sync")
        .ok(RoomMessagesResponseTemplate::default()
            .end_token("token-before-messages")
            .events(prev_events))
        .named("messages")
        .mount()
        .await;
    // Mock /messages in case we use no token.
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default()
            .end_token("token-before-sync-from-messages")
            .events(sync_events.clone().into_iter().rev().collect()))
        .named("messages")
        .mount()
        .await;

    // Spawn back pagination.
    let pagination = room_event_cache.pagination();
    let back_pagination_handle =
        spawn(async move { pagination.run_backwards_once(3).await.unwrap() });

    // Receive a non-limited sync while back pagination is happening.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .set_timeline_prev_batch("token-before-sync-from-sync")
                .add_timeline_bulk(sync_events.into_iter().map(ruma::serde::Raw::cast)),
        )
        .await;

    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );
    assert_eq!(diffs.len(), 1);
    assert_let!(VectorDiff::Append { values } = &diffs[0]);

    assert_eq!(values.len(), 3);
    assert_event_matches_msg(&values[0], "sync1");
    assert_event_matches_msg(&values[1], "sync2");
    assert_event_matches_msg(&values[2], "sync3");

    // Back pagination should succeed, and we haven't reached the start.
    let outcome = back_pagination_handle.await.unwrap();
    assert!(outcome.reached_start.not());
    assert_eq!(outcome.events.len(), 3);

    // And the back-paginated events come down from the subscriber too.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
    );
    assert_eq!(diffs.len(), 3);
    assert_let!(VectorDiff::Insert { index: 0, value: _ } = &diffs[0]);
    assert_let!(VectorDiff::Insert { index: 1, value: _ } = &diffs[1]);
    assert_let!(VectorDiff::Insert { index: 2, value: _ } = &diffs[2]);

    assert!(subscriber.is_empty());
}

#[async_test]
async fn test_relations_ordering() {
    let server = MatrixMockServer::new().await;

    let room_id = room_id!("!galette:saucisse.bzh");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    let target_event_id = event_id!("$1");

    // Start with a prefilled event cache store that includes the target event.
    let ev1 = f.text_msg("bonjour monde").event_id(target_event_id).into_event();

    let event_cache_store = Arc::new(MemoryStore::new());
    event_cache_store
        .handle_linked_chunk_updates(
            LinkedChunkId::Room(room_id),
            vec![
                // An empty items chunk.
                Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(0), next: None },
                Update::PushItems {
                    at: Position::new(ChunkIdentifier::new(0), 0),
                    items: vec![ev1.clone()],
                },
            ],
        )
        .await
        .unwrap();

    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.store_config(
                StoreConfig::new("hodlor".to_owned()).event_cache_store(event_cache_store.clone()),
            )
        })
        .build()
        .await;

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room = server.sync_joined_room(&client, room_id).await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (initial_events, mut listener) = room_event_cache.subscribe().await.unwrap();
    assert_eq!(initial_events.len(), 1);
    assert!(listener.recv().now_or_never().is_none());

    // Sanity check: there are no relations for the target event yet.
    let (_, relations) =
        room_event_cache.find_event_with_relations(target_event_id, None).await.unwrap().unwrap();
    assert!(relations.is_empty());

    let edit2 = event_id!("$edit2");
    let ev2 = f
        .text_msg("* hola mundo")
        .edit(target_event_id, RoomMessageEventContentWithoutRelation::text_plain("hola mundo"))
        .event_id(edit2)
        .into_raw();

    let edit3 = event_id!("$edit3");
    let ev3 = f
        .text_msg("* ciao mondo")
        .edit(target_event_id, RoomMessageEventContentWithoutRelation::text_plain("ciao mondo"))
        .event_id(edit3);

    let edit4 = event_id!("$edit4");
    let ev4 = f
        .text_msg("* hello world")
        .edit(target_event_id, RoomMessageEventContentWithoutRelation::text_plain("hello world"))
        .event_id(edit4);

    // We receive two edit events via sync, as well as a gap; this will shrink the
    // linked chunk.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(ev3)
                .add_timeline_event(ev4)
                .set_timeline_limited()
                .set_timeline_prev_batch("prev_batch"),
        )
        .await;

    // Wait for the listener to tell us we've received something.
    loop {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = listener.recv()
        );
        // We've received the shrink.
        if diffs.iter().any(|diff| matches!(diff, VectorDiff::Clear)) {
            break;
        }
    }

    // At this point, relations are known for the target event.
    let (_, relations) =
        room_event_cache.find_event_with_relations(target_event_id, None).await.unwrap().unwrap();
    assert_eq!(relations.len(), 2);
    // And the edit events are correctly ordered according to their position in the
    // linked chunk.
    assert_eq!(relations[0].event_id().unwrap(), edit3);
    assert_eq!(relations[1].event_id().unwrap(), edit4);

    // Now, we resolve the gap; this returns ev2, another edit.
    server
        .mock_room_messages()
        .match_from("prev_batch")
        .ok(RoomMessagesResponseTemplate::default().events(vec![ev2.clone()]))
        .named("room/messages")
        .mock_once()
        .mount()
        .await;

    // Run the pagination.
    let outcome = room_event_cache.pagination().run_backwards_once(1).await.unwrap();
    assert!(outcome.reached_start.not());
    assert_eq!(outcome.events.len(), 1);

    {
        // Sanity check: we load the first chunk with the first event, from disk, and
        // reach the start of the timeline.
        let outcome = room_event_cache.pagination().run_backwards_once(1).await.unwrap();
        assert!(outcome.reached_start);
    }

    // Relations are returned accordingly.
    let (_, relations) =
        room_event_cache.find_event_with_relations(target_event_id, None).await.unwrap().unwrap();
    assert_eq!(relations.len(), 3);
    assert_eq!(relations[0].event_id().unwrap(), edit2);
    assert_eq!(relations[1].event_id().unwrap(), edit3);
    assert_eq!(relations[2].event_id().unwrap(), edit4);

    // If I save an additional event without storing it in the linked chunk, it will
    // be present at the start of the relations list.
    let edit5 = event_id!("$edit5");
    let ev5 = f
        .text_msg("* hallo Welt")
        .edit(target_event_id, RoomMessageEventContentWithoutRelation::text_plain("hallo Welt"))
        .event_id(edit5)
        .into_event();

    server.mock_room_event().ok(ev5).mock_once().mount().await;

    // This saves the event, but without a position.
    room.event(edit5, None).await.unwrap();

    let (_, relations) =
        room_event_cache.find_event_with_relations(target_event_id, None).await.unwrap().unwrap();
    assert_eq!(relations.len(), 4);
    assert_eq!(relations[0].event_id().unwrap(), edit5);
    assert_eq!(relations[1].event_id().unwrap(), edit2);
    assert_eq!(relations[2].event_id().unwrap(), edit3);
    assert_eq!(relations[3].event_id().unwrap(), edit4);
}
