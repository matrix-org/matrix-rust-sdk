use std::time::Duration;

use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use imbl::Vector;
use matrix_sdk::{
    Client, ThreadingSupport, assert_let_timeout,
    deserialized_responses::{ThreadSummaryStatus, TimelineEvent},
    event_cache::{RoomEventCacheSubscriber, RoomEventCacheUpdate, ThreadEventCacheUpdate},
    sleep::sleep,
    test_utils::{
        assert_event_matches_msg,
        mocks::{MatrixMockServer, RoomRelationsResponseTemplate},
    },
};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{
    OwnedEventId, OwnedRoomId, event_id,
    events::{AnySyncTimelineEvent, Mentions},
    push::{ConditionalPushRule, Ruleset},
    room_id,
    serde::Raw,
    user_id,
};
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

async fn client_with_threading_support(server: &MatrixMockServer) -> Client {
    server
        .client_builder()
        .on_builder(|builder| {
            builder.with_threading_support(ThreadingSupport::Enabled { with_subscriptions: false })
        })
        .build()
        .await
}

#[async_test]
async fn test_thread_can_paginate_even_if_seen_sync_event() {
    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

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
        room_event_cache.subscribe_to_thread(thread_root_id.to_owned()).await.unwrap();

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
    let client = client_with_threading_support(&server).await;

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
        room_event_cache.subscribe_to_thread(thread_root.to_owned()).await.unwrap();

    // Then, at first, the thread contains the two initial events.
    let events = wait_for_initial_events(events, &mut thread_stream).await;
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
    let client = client_with_threading_support(&server).await;

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
        room_event_cache.subscribe_to_thread(thread_root1.to_owned()).await.unwrap();

    assert!(thread1_events.is_empty());
    assert!(thread1_stream.is_empty());

    let (thread2_events, mut thread2_stream) =
        room_event_cache.subscribe_to_thread(thread_root2.to_owned()).await.unwrap();

    assert!(thread2_events.is_empty());
    assert!(thread2_stream.is_empty());

    // Also subscribe to the room.
    let (room_events, mut room_stream) = room_event_cache.subscribe().await.unwrap();

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

    let reloaded_thread1 = room_event_cache.find_event(thread_root1).await.unwrap().unwrap();
    assert_let!(ThreadSummaryStatus::Some(summary) = reloaded_thread1.thread_summary);
    assert_eq!(summary.num_replies, 2);
    assert!(summary.latest_reply.is_none());

    // That's all, folks!
    assert!(thread1_stream.is_empty());
}

#[async_test]
async fn test_deduplication() {
    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

    // Immediately subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");

    let f = EventFactory::new().room(room_id).sender(*ALICE);

    let thread_root = event_id!("$thread_root");
    let first_reply_event_id = event_id!("$first_reply");
    let first_reply = f
        .text_msg("hey there")
        .in_thread(thread_root, thread_root)
        .event_id(first_reply_event_id)
        .into_raw_timeline();
    let second_reply_event_id = event_id!("$second_reply");
    let second_reply = f
        .text_msg("hoy!")
        .in_thread(thread_root, first_reply_event_id)
        .event_id(second_reply_event_id)
        .into_raw_timeline();

    // Given a room with a thread, that has two replies.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_bulk(vec![first_reply.clone().cast(), second_reply.clone().cast()]),
        )
        .await;

    // And we subscribe to the thread,
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
    let (events, mut thread_stream) =
        room_event_cache.subscribe_to_thread(thread_root.to_owned()).await.unwrap();

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
            JoinedRoomBuilder::new(room_id).add_timeline_bulk(vec![
                f.text_msg("hoy!")
                    .in_thread(thread_root, first_reply_event_id)
                    .event_id(second_reply_event_id)
                    .into_raw_sync(),
            ]),
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
            .events(vec![first_reply, second_reply])
            .next_batch("next_batch"))
        .mock_once()
        .mount()
        .await;

    room_event_cache.paginate_thread_backwards(thread_root.to_owned(), 42).await.unwrap();

    // The events were already known, so the stream is still empty.
    assert!(thread_stream.is_empty());
}

struct ThreadSubscriptionTestSetup {
    server: MatrixMockServer,
    client: Client,
    factory: EventFactory,
    room_id: OwnedRoomId,
    subscriber: RoomEventCacheSubscriber,
    /// 3 events: 1 non-mention, 1 mention, and another non-mention.
    events: Vec<Raw<AnySyncTimelineEvent>>,
    mention_event_id: OwnedEventId,
    thread_root: OwnedEventId,
}

/// Create a new setup for a thread subscription test, with enough data so that
/// a push context can be created.
///
/// The setup uses custom push rules, to trigger notifications only on mentions.
///
/// The setup includes 3 events (1 non-mention, 1 mention, and another
/// non-mention) in the same thread, for easy testing of automated
/// subscriptions.
async fn thread_subscription_test_setup() -> ThreadSubscriptionTestSetup {
    let server = MatrixMockServer::new().await;

    let thread_root = event_id!("$thread_root");

    // Assuming a client that's interested in thread subscriptions,
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
        })
        .build()
        .await;

    // Immediately subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!omelette:fromage.fr");
    let room = server.sync_joined_room(&client, room_id).await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (initial_events, mut subscriber) = room_event_cache.subscribe().await.unwrap();
    assert!(initial_events.is_empty());
    assert!(subscriber.is_empty());

    // Provide a dummy sync with the room's member profile of the current user, so
    // the push context can be created.
    let own_user_id = client.user_id().unwrap();
    let f = EventFactory::new().room(room_id).sender(*ALICE);
    let member = f.member(own_user_id).sender(own_user_id);

    // Override push rules so that only an intentional mention causes a
    // notification.
    let mut push_rules = Ruleset::default();
    push_rules.override_.insert(ConditionalPushRule::is_user_mention(own_user_id));

    server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_state_event(member));
            sync_builder.add_global_account_data(f.push_rules(push_rules));
        })
        .await;

    // Wait for the initial sync processing to complete; it will trigger a member
    // update, at the very least.
    assert_let_timeout!(Ok(RoomEventCacheUpdate::UpdateMembers { .. }) = subscriber.recv());

    let first_reply_event_id = event_id!("$first_reply");
    let first_reply = f
        .text_msg("hey there")
        .in_thread(thread_root, thread_root)
        .event_id(first_reply_event_id)
        .into();

    let second_reply_event_id = event_id!("$second_reply");
    let second_reply = f
        .text_msg("hoy test user!")
        .mentions(Mentions::with_user_ids([own_user_id.to_owned()]))
        .in_thread(thread_root, first_reply_event_id)
        .event_id(second_reply_event_id)
        .into();

    let third_reply_event_id = event_id!("$third_reply");
    let third_reply = f
        .text_msg("ciao!")
        .in_thread(thread_root, second_reply_event_id)
        .event_id(third_reply_event_id)
        .into();

    ThreadSubscriptionTestSetup {
        server,
        client,
        factory: f,
        subscriber,
        events: vec![first_reply, second_reply, third_reply],
        mention_event_id: second_reply_event_id.to_owned(),
        thread_root: thread_root.to_owned(),
        room_id: room_id.to_owned(),
    }
}

#[async_test]
async fn test_auto_subscribe_thread_via_sync() {
    let mut s = thread_subscription_test_setup().await;

    // (The endpoint will be called for the current thread, and with an automatic
    // subscription up to the given event ID.)
    s.server
        .mock_room_put_thread_subscription()
        .match_automatic_event_id(&s.mention_event_id)
        .match_thread_id(s.thread_root.to_owned())
        .ok()
        .mock_once()
        .mount()
        .await;

    let mut thread_subscriber_updates =
        s.client.event_cache().subscribe_thread_subscriber_updates();

    // When I receive 3 events (1 non mention, 1 mention, then 1 non mention again),
    // from sync, I'll get subscribed to the thread because of the second event.
    s.server
        .sync_room(&s.client, JoinedRoomBuilder::new(&s.room_id).add_timeline_bulk(s.events))
        .await;

    // Let the event cache process the update.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = s.subscriber.recv()
    );
    assert_let_timeout!(Ok(()) = thread_subscriber_updates.recv());

    // The actual check is the `mock_once` call above!
}

#[async_test]
async fn test_dont_auto_subscribe_on_already_subscribed_thread() {
    let mut s = thread_subscription_test_setup().await;

    // Given a thread I'm already subscribed to,
    s.server
        .mock_room_get_thread_subscription()
        .match_thread_id(s.thread_root.to_owned())
        .ok(false)
        .mock_once()
        .mount()
        .await;

    // The PUT endpoint (to subscribe to the thread) shouldn't be called…
    s.server.mock_room_put_thread_subscription().ok().expect(0).mount().await;

    // …when I receive a new in-thread mention for this thread.
    s.server
        .sync_room(&s.client, JoinedRoomBuilder::new(&s.room_id).add_timeline_bulk(s.events))
        .await;

    // Let the event cache process the update.
    assert_let_timeout!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents { .. }) = s.subscriber.recv()
    );

    // Let a bit of time for the background thread subscriber task to process the
    // update.
    sleep(Duration::from_millis(200)).await;

    // The actual check is the `expect` call above!
}

#[async_test]
async fn test_auto_subscribe_on_thread_paginate() {
    // In this scenario, we're back-paginating a thread and making sure that the
    // back-paginated events do cause a subscription.

    let s = thread_subscription_test_setup().await;

    let event_cache = s.client.event_cache();
    event_cache.subscribe().unwrap();

    let mut thread_subscriber_updates =
        s.client.event_cache().subscribe_thread_subscriber_updates();

    let thread_root_id = event_id!("$thread_root");
    let thread_resp_id = event_id!("$thread_resp");

    // Receive an in-thread event.
    let room = s
        .server
        .sync_room(
            &s.client,
            JoinedRoomBuilder::new(&s.room_id).add_timeline_event(
                s.factory
                    .text_msg("that's a good point")
                    .in_thread(thread_root_id, thread_root_id)
                    .event_id(thread_resp_id),
            ),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (thread_events, mut thread_stream) =
        room_event_cache.subscribe_to_thread(thread_root_id.to_owned()).await.unwrap();

    // Sanity check: the sync event is added to the thread.
    let mut thread_events = wait_for_initial_events(thread_events, &mut thread_stream).await;
    assert_eq!(thread_events.len(), 1);
    assert_eq!(thread_events.remove(0).event_id().as_deref(), Some(thread_resp_id));

    assert!(thread_subscriber_updates.is_empty());

    // It's possible to paginate the thread, and this will push the thread root
    // because there's no prev-batch token.
    let reversed_events = s.events.into_iter().rev().map(Raw::cast_unchecked).collect();
    s.server
        .mock_room_relations()
        .match_target_event(thread_root_id.to_owned())
        .ok(RoomRelationsResponseTemplate::default().events(reversed_events))
        .mock_once()
        .mount()
        .await;

    s.server
        .mock_room_event()
        .match_event_id()
        .ok(s.factory.text_msg("Thread root").event_id(thread_root_id).into())
        .mock_once()
        .mount()
        .await;

    // (The endpoint will be called for the current thread, and with an automatic
    // subscription up to the given event ID.)
    s.server
        .mock_room_put_thread_subscription()
        .match_automatic_event_id(&s.mention_event_id)
        .match_thread_id(s.thread_root.to_owned())
        .ok()
        .mock_once()
        .mount()
        .await;

    let hit_start =
        room_event_cache.paginate_thread_backwards(thread_root_id.to_owned(), 42).await.unwrap();
    assert!(hit_start);

    // Let the event cache process the update.
    assert_let_timeout!(Ok(ThreadEventCacheUpdate { .. }) = thread_stream.recv());
    assert_let_timeout!(Ok(()) = thread_subscriber_updates.recv());
    assert!(thread_subscriber_updates.is_empty());
}

#[async_test]
async fn test_auto_subscribe_on_thread_paginate_root_event() {
    // In this scenario, the root of a thread is the event that would cause the
    // subscription.

    let s = thread_subscription_test_setup().await;

    let event_cache = s.client.event_cache();
    event_cache.subscribe().unwrap();

    let mut thread_subscriber_updates =
        s.client.event_cache().subscribe_thread_subscriber_updates();

    let thread_root_id = event_id!("$thread_root");
    let thread_resp_id = event_id!("$thread_resp");

    // Receive an in-thread event.
    let room = s
        .server
        .sync_room(
            &s.client,
            JoinedRoomBuilder::new(&s.room_id).add_timeline_event(
                s.factory
                    .text_msg("that's a good point")
                    .in_thread(thread_root_id, thread_root_id)
                    .event_id(thread_resp_id),
            ),
        )
        .await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (thread_events, mut thread_stream) =
        room_event_cache.subscribe_to_thread(thread_root_id.to_owned()).await.unwrap();

    // Sanity check: the sync event is added to the thread.
    let mut thread_events = wait_for_initial_events(thread_events, &mut thread_stream).await;
    assert_eq!(thread_events.len(), 1);
    assert_eq!(thread_events.remove(0).event_id().as_deref(), Some(thread_resp_id));

    assert!(thread_subscriber_updates.is_empty());

    // It's possible to paginate the thread, and this will push the thread root
    // because there's no prev-batch token.
    s.server
        .mock_room_relations()
        .match_target_event(thread_root_id.to_owned())
        .ok(RoomRelationsResponseTemplate::default())
        .mock_once()
        .mount()
        .await;

    s.server
        .mock_room_event()
        .match_event_id()
        .ok(s
            .factory
            .text_msg("da r00t")
            .event_id(thread_root_id)
            .mentions(Mentions::with_user_ids(s.client.user_id().map(ToOwned::to_owned)))
            .into())
        .mock_once()
        .mount()
        .await;

    // (The endpoint will be called for the current thread, and with an automatic
    // subscription up to the given event ID.)
    s.server
        .mock_room_put_thread_subscription()
        .match_automatic_event_id(thread_root_id)
        .match_thread_id(thread_root_id.to_owned())
        .ok()
        .mock_once()
        .mount()
        .await;

    let hit_start =
        room_event_cache.paginate_thread_backwards(thread_root_id.to_owned(), 42).await.unwrap();
    assert!(hit_start);

    // Let the event cache process the update.
    assert_let_timeout!(Ok(ThreadEventCacheUpdate { .. }) = thread_stream.recv());
    assert_let_timeout!(Ok(()) = thread_subscriber_updates.recv());
}

#[async_test]
async fn test_redact_touches_threads() {
    // We start with a thread with some replies, then receive redactions for those
    // replies over sync. We observe that the thread linked chunks are correctly
    // updated, as well as the thread summary on the thread root event.

    let s = thread_subscription_test_setup().await;
    let f = s.factory;

    let event_cache = s.client.event_cache();
    event_cache.subscribe().unwrap();

    let thread_root_id = s.thread_root;
    let thread_resp1 = s.events[0].get_field::<OwnedEventId>("event_id").unwrap().unwrap();
    let thread_resp2 = s.events[1].get_field::<OwnedEventId>("event_id").unwrap().unwrap();

    let room = s.server.sync_joined_room(&s.client, &s.room_id).await;

    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    let (thread_events, mut thread_stream) =
        room_event_cache.subscribe_to_thread(thread_root_id.to_owned()).await.unwrap();

    // Receive a thread root, and a threaded reply.
    s.server
        .sync_room(
            &s.client,
            JoinedRoomBuilder::new(&s.room_id)
                .add_timeline_event(f.text_msg("da r00t").event_id(&thread_root_id))
                .add_timeline_event(s.events[0].clone())
                .add_timeline_event(s.events[1].clone()),
        )
        .await;

    // Sanity check: both events are present in the thread, and the thread summary
    // is correct.
    let mut thread_events = wait_for_initial_events(thread_events, &mut thread_stream).await;
    assert_eq!(thread_events.len(), 3);
    assert_eq!(thread_events.remove(0).event_id().as_ref(), Some(&thread_root_id));
    assert_eq!(thread_events.remove(0).event_id().as_ref(), Some(&thread_resp1));
    assert_eq!(thread_events.remove(0).event_id().as_ref(), Some(&thread_resp2));

    let (room_events, mut room_stream) = room_event_cache.subscribe().await.unwrap();
    assert_eq!(room_events.len(), 3);

    {
        assert_eq!(room_events[0].event_id().as_ref(), Some(&thread_root_id));
        let summary = room_events[0].thread_summary.summary().unwrap();
        assert_eq!(summary.latest_reply.as_ref(), Some(&thread_resp2));
        assert_eq!(summary.num_replies, 2);
    }

    assert_eq!(room_events[1].event_id().as_ref(), Some(&thread_resp1));
    assert_eq!(room_events[2].event_id().as_ref(), Some(&thread_resp2));

    assert!(thread_stream.is_empty());
    assert!(room_stream.is_empty());

    // A redaction for the first reply comes through sync.
    let thread_resp1_redaction = event_id!("$redact_thread_resp1");
    s.server
        .sync_room(
            &s.client,
            JoinedRoomBuilder::new(&s.room_id)
                .add_timeline_event(f.redaction(&thread_resp1).event_id(thread_resp1_redaction)),
        )
        .await;

    // The redaction affects the thread cache: it *removes* the redacted event.
    {
        assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread_stream.recv());
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Remove { index: 1 } = &diffs[0]);

        assert!(thread_stream.is_empty());
    }

    // The redaction affects the room cache too:
    // - the redaction event is pushed to the room history,
    // - the redaction's target is, well, redacted,
    // - the thread summary is updated correctly.
    {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
        );
        assert_eq!(diffs.len(), 3);

        // The redaction event is appended to the room cache.
        assert_let!(VectorDiff::Append { values: new_events } = &diffs[0]);
        assert_eq!(new_events.len(), 1);
        assert_eq!(new_events[0].event_id().as_deref(), Some(thread_resp1_redaction));

        // The room event is redacted.
        {
            assert_let!(VectorDiff::Set { index: 1, value: new_event } = &diffs[1]);
            let deserialized = new_event.raw().deserialize().unwrap();

            // TODO: replace with https://github.com/ruma/ruma/pull/2254 when it's been merged in Ruma.
            assert!(match deserialized {
                AnySyncTimelineEvent::MessageLike(ev) => ev.is_redacted(),
                AnySyncTimelineEvent::State(_ev) => unreachable!(),
            });
        }

        // The thread summary is updated.
        {
            assert_let!(VectorDiff::Set { index: 0, value: new_root } = &diffs[2]);
            assert_eq!(new_root.event_id().as_ref(), Some(&thread_root_id));
            let summary = new_root.thread_summary.summary().unwrap();
            assert_eq!(summary.latest_reply.as_ref(), Some(&thread_resp2));
            assert_eq!(summary.num_replies, 1);
        }
    }

    // A redaction for the second (and last) reply comes through sync.
    let thread_resp2_redaction = event_id!("$redact_thread_resp2");
    s.server
        .sync_room(
            &s.client,
            JoinedRoomBuilder::new(&s.room_id)
                .add_timeline_event(f.redaction(&thread_resp2).event_id(thread_resp2_redaction)),
        )
        .await;

    // The redaction affects the thread cache: it *removes* the redacted event.
    {
        assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread_stream.recv());
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Remove { index: 1 } = &diffs[0]);

        assert!(thread_stream.is_empty());
    }

    // The redaction affects the room cache too:
    // - the redaction event is pushed to the room history,
    // - the redaction's target is, well, redacted,
    // - the thread summary is removed from the thread root.
    {
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = room_stream.recv()
        );
        assert_eq!(diffs.len(), 3);

        // The redaction event is appended to the room cache.
        assert_let!(VectorDiff::Append { values: new_events } = &diffs[0]);
        assert_eq!(new_events.len(), 1);
        assert_eq!(new_events[0].event_id().as_deref(), Some(thread_resp2_redaction));

        // The room event is redacted.
        {
            assert_let!(VectorDiff::Set { index: 2, value: new_event } = &diffs[1]);
            let deserialized = new_event.raw().deserialize().unwrap();

            // TODO: replace with https://github.com/ruma/ruma/pull/2254 when it's been merged in Ruma.
            assert!(match deserialized {
                AnySyncTimelineEvent::MessageLike(ev) => ev.is_redacted(),
                AnySyncTimelineEvent::State(_ev) => unreachable!(),
            });
        }

        // The thread summary is removed.
        {
            assert_let!(VectorDiff::Set { index: 0, value: new_root } = &diffs[2]);
            assert_eq!(new_root.event_id().as_ref(), Some(&thread_root_id));
            assert!(new_root.thread_summary.summary().is_none());
        }
    }
}
