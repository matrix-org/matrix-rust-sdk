use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk::{
    assert_let_timeout,
    event_cache::ThreadEventCacheUpdate,
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

    let (mut thread_events, mut thread_stream) =
        room_event_cache.subscribe_to_thread(thread_root_id.to_owned()).await;

    // Sanity check: the sync event is added to the thread. This is racy because the
    // update might not have been handled by the event cache yet.
    let first_event = if thread_events.is_empty() {
        assert_let_timeout!(Ok(ThreadEventCacheUpdate { diffs, .. }) = thread_stream.recv());
        assert_eq!(diffs.len(), 1);
        let mut diffs = diffs;
        assert_let!(VectorDiff::Append { values } = diffs.remove(0));
        assert_eq!(values.len(), 1);
        let mut values = values;
        values.remove(0)
    } else {
        assert_eq!(thread_events.len(), 1);
        thread_events.remove(0)
    };
    assert_eq!(first_event.event_id().as_deref(), Some(thread_resp_id));

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
