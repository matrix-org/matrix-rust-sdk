use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk::{
    assert_let_timeout,
    event_cache::ThreadEventCacheUpdate,
    test_utils::mocks::{MatrixMockServer, RoomRelationsResponseTemplate},
};
use matrix_sdk_test::{async_test, event_factory::EventFactory, JoinedRoomBuilder, ALICE};
use ruma::{event_id, room_id};

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
