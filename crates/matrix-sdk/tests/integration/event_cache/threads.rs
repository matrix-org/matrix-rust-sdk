use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use imbl::Vector;
use matrix_sdk::{
    assert_let_timeout,
    deserialized_responses::{ThreadSummaryStatus, TimelineEvent},
    event_cache::{RoomEventCacheUpdate, ThreadEventCacheUpdate},
    test_utils::{
        assert_event_matches_msg,
        mocks::{MatrixMockServer, RoomRelationsResponseTemplate},
    },
};
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, GlobalAccountDataTestEvent, JoinedRoomBuilder, ALICE,
};
use ruma::{event_id, room_id, user_id};
use serde_json::json;
use tokio::sync::broadcast;

/// Small helper for backpagination tests, to wait for initial events to
/// stabilize.
async fn wait_for_initial_events(
    mut events: Vec<TimelineEvent>,
    stream: &mut broadcast::Receiver<ThreadEventCacheUpdate>,
) -> Vec<TimelineEvent> {
    if events.is_empty() {
        // Wait for a first update.
        let mut vector = Vector::new();

        assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = stream.recv());

        for diff in diffs {
            diff.apply(&mut vector);
        }

        events.extend(vector);
        events
    } else {
        events
    }
}

#[async_test]
async fn test_thread_can_paginate_even_if_seen_sync_event() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!galette:saucisse.bzh");

    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let thread_root_id = event_id!("$thread_root");
    let thread_resp_id = event_id!("$thread_resp");

    // Receive an in-thread event.
    let f = EventFactory::new().room(room_id).sender(*ALICE);
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("that's a good point")
                    .in_thread(thread_root_id, thread_root_id)
                    .event_id(thread_resp_id),
            ),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (thread_events, mut thread_stream) =
        room_event_cache.subscribe_to_thread(thread_root_id.to_owned()).await;

    // Sanity check: the sync event is added to the thread.
    let mut thread_events = wait_for_initial_events(thread_events, &mut thread_stream).await;
    assert_eq!(thread_events.len(), 1);
    assert_eq!(thread_events.remove(0).event_id().as_deref(), Some(thread_resp_id));

    // It's possible to paginate the thread, and this will push the thread root
    // because there's no prev-batch token.
    server
        .mock_room_relations()
        .match_target_event(thread_root_id.to_owned())
        .ok(RoomRelationsResponseTemplate::default())
        .mock_once()
        .mount()
        .await;

    server
        .mock_room_event()
        .match_event_id()
        .ok(f.text_msg("Thread root").event_id(thread_root_id).into())
        .mock_once()
        .mount()
        .await;

    let hit_start =
        room_event_cache.paginate_thread_backwards(thread_root_id.to_owned(), 42).await.unwrap();
    assert!(hit_start);

    assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread_stream.recv());
    assert_eq!(diffs.len(), 1);
    assert_let!(VectorDiff::Insert { index: 0, value } = &diffs[0]);
    assert_eq!(value.event_id().as_deref(), Some(thread_root_id));
}

#[async_test]
async fn test_ignored_user_empties_threads() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Immediately subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let dexter = user_id!("@dexter:lab.org");
    let ivan = user_id!("@ivan:lab.ch");

    let f = EventFactory::new();

    let thread_root = event_id!("$thread_root");
    let first_reply_event_id = event_id!("$first_reply");
    let second_reply_event_id = event_id!("$second_reply");

    // Given a room with a thread, that has two replies.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                f.text_msg("hey there")
                    .sender(dexter)
                    .in_thread(thread_root, thread_root)
                    .event_id(first_reply_event_id)
                    .into_raw_sync(),
                f.text_msg("hoy!")
                    .sender(ivan)
                    .in_thread(thread_root, first_reply_event_id)
                    .event_id(second_reply_event_id)
                    .into_raw_sync(),
            ]),
        )
        .await;

    // And we subscribe to the thread,
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (events, mut thread_stream) =
        room_event_cache.subscribe_to_thread(thread_root.to_owned()).await;

    // Then, at first, the thread contains the two initial events.
    let events = wait_for_initial_events(events, &mut thread_stream).await;
    assert_eq!(events.len(), 2);
    assert_event_matches_msg(&events[0], "hey there");
    assert_event_matches_msg(&events[1], "hoy!");

    // `dexter` is ignored.
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

    // We do receive a clear.
    {
        assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread_stream.recv());
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Clear = &diffs[0]);
    }

    // Receiving new events still works.
    server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_joined_room(
                JoinedRoomBuilder::new(room_id).add_timeline_event(
                    f.text_msg("i don't like this dexter")
                        .in_thread(thread_root, second_reply_event_id)
                        .sender(ivan),
                ),
            );
        })
        .await;

    // We do receive the new event.
    {
        assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread_stream.recv());
        assert_eq!(diffs.len(), 1);

        assert_let!(VectorDiff::Append { values: events } = &diffs[0]);
        assert_eq!(events.len(), 1);
        assert_event_matches_msg(&events[0], "i don't like this dexter");
    }

    // That's all, folks!
    assert!(thread_stream.is_empty());
}

#[async_test]
async fn test_gappy_sync_empties_all_threads() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Immediately subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().sender(*ALICE);

    let thread_root1 = event_id!("$t1root");
    let thread1_reply1 = event_id!("$t1ev1");
    let thread1_reply2 = event_id!("$t1ev2");

    let thread_root2 = event_id!("$t2root");
    let thread2_reply1 = event_id!("$t2ev1");

    // We subscribe to each thread (before the initial sync, so the state is stable
    // before running checks).
    let room = server.sync_joined_room(&client, room_id).await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (thread1_events, mut thread1_stream) =
        room_event_cache.subscribe_to_thread(thread_root1.to_owned()).await;

    assert!(thread1_events.is_empty());
    assert!(thread1_stream.is_empty());

    let (thread2_events, mut thread2_stream) =
        room_event_cache.subscribe_to_thread(thread_root2.to_owned()).await;

    assert!(thread2_events.is_empty());
    assert!(thread2_stream.is_empty());

    // Also subscribe to the room.
    let (room_events, mut room_stream) = room_event_cache.subscribe().await;

    assert!(room_events.is_empty());
    assert!(room_stream.is_empty());

    // Given a room with two threads, each having some replies,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                // First thread root.
                f.text_msg("say hi in thread").event_id(thread_root1).into_raw_sync(),
                // Second thread root.
                f.text_msg("say bye in thread").event_id(thread_root2).into_raw_sync(),
                // Reply to the first thread.
                f.text_msg("hey there")
                    .in_thread(thread_root1, thread_root1)
                    .event_id(thread1_reply1)
                    .into_raw_sync(),
                // Reply to the second thread.
                f.text_msg("bye there")
                    .in_thread(thread_root2, thread_root2)
                    .event_id(thread2_reply1)
                    .into_raw_sync(),
                // Another reply to the first thread.
                f.text_msg("hoy!")
                    .in_thread(thread_root1, thread1_reply1)
                    .event_id(thread1_reply2)
                    .into_raw_sync(),
            ]),
        )
        .await;

    // The first thread contains all thread events, including the root.
    assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread1_stream.recv());
    assert_eq!(diffs.len(), 1);
    assert_let!(VectorDiff::Append { values: thread1_events } = &diffs[0]);
    assert_eq!(thread1_events.len(), 3);
    assert_event_matches_msg(&thread1_events[0], "say hi in thread");
    assert_event_matches_msg(&thread1_events[1], "hey there");
    assert_event_matches_msg(&thread1_events[2], "hoy!");

    // The second thread contains the in-thread event (but not the root, for the
    // same reason as above).
    assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread2_stream.recv());
    assert_eq!(diffs.len(), 1);
    assert_let!(VectorDiff::Append { values: thread2_events } = &diffs[0]);
    assert_eq!(thread2_events.len(), 2);
    assert_event_matches_msg(&thread2_events[0], "say bye in thread");
    assert_event_matches_msg(&thread2_events[1], "bye there");

    // The room contains all five events.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
    );
    assert_eq!(diffs.len(), 3);
    assert_let!(VectorDiff::Append { values: room_events } = &diffs[0]);
    assert_eq!(room_events.len(), 5);
    // Two thread summary updates, for the thread roots.
    assert_let!(VectorDiff::Set { index: 0, .. } = &diffs[1]);
    assert_let!(VectorDiff::Set { index: 1, .. } = &diffs[2]);

    // Receive a gappy sync for the room, with no events (so the event cache doesn't
    // filter the gap out).
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .set_timeline_limited()
                .set_timeline_prev_batch("prev_batch"),
        )
        .await;

    // Both threads are cleared.
    {
        assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread1_stream.recv());
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Clear = &diffs[0]);
    }
    {
        assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread2_stream.recv());
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Clear = &diffs[0]);
    }

    // The room is shrunk to the gap.
    {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Clear = &diffs[0]);
    }

    // If we manually load the root events from the db, the summaries have been
    // updated too:
    // - the count is still the same, as it's based on the number of replies known
    //   in the room linked chunk (that's persisted on disk). It might be outdated,
    //   but that's a lower bound.
    // - but the latest reply is now `None`, as we're not sure that the latest reply
    //   is still the latest one.

    let reloaded_thread1 = room_event_cache.find_event(thread_root1).await.unwrap();
    assert_let!(ThreadSummaryStatus::Some(summary) = reloaded_thread1.thread_summary);
    assert_eq!(summary.num_replies, 2);
    assert!(summary.latest_reply.is_none());

    // That's all, folks!
    assert!(thread1_stream.is_empty());
}

#[async_test]
async fn test_deduplication() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Immediately subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().sender(*ALICE);

    let thread_root = event_id!("$thread_root");
    let first_reply_event_id = event_id!("$first_reply");
    let first_reply = f
        .text_msg("hey there")
        .in_thread(thread_root, thread_root)
        .event_id(first_reply_event_id)
        .into_event();
    let second_reply_event_id = event_id!("$second_reply");
    let second_reply = f
        .text_msg("hoy!")
        .in_thread(thread_root, first_reply_event_id)
        .event_id(second_reply_event_id)
        .into_event();

    // Given a room with a thread, that has two replies.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_bulk(vec![first_reply.raw().clone(), second_reply.raw().clone()]),
        )
        .await;

    // And we subscribe to the thread,
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (events, mut thread_stream) =
        room_event_cache.subscribe_to_thread(thread_root.to_owned()).await;

    // Then, at first, the thread contains the two initial events.
    let events = wait_for_initial_events(events, &mut thread_stream).await;
    assert_eq!(events.len(), 2);
    assert_event_matches_msg(&events[0], "hey there");
    assert_event_matches_msg(&events[1], "hoy!");

    // No updates on the stream.
    assert!(thread_stream.is_empty());

    // We receive a sync with the same tail events.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![f
                .text_msg("hoy!")
                .in_thread(thread_root, first_reply_event_id)
                .event_id(second_reply_event_id)
                .into_raw_sync()]),
        )
        .await;

    // Still no updates on the stream: the event has been deduplicated, and there
    // were no gaps.
    assert!(thread_stream.is_empty());

    // If I backpaginate in that thread, and the pagination only returns events I
    // already knew about, the stream is still empty.
    server
        .mock_room_relations()
        .match_target_event(thread_root.to_owned())
        .ok(RoomRelationsResponseTemplate::default()
            .events(vec![
                first_reply.raw().cast_ref().clone(),
                second_reply.raw().cast_ref().clone(),
            ])
            .next_batch("next_batch"))
        .mock_once()
        .mount()
        .await;

    room_event_cache.paginate_thread_backwards(thread_root.to_owned(), 42).await.unwrap();

    // The events were already known, so the stream is still empty.
    assert!(thread_stream.is_empty());
}
