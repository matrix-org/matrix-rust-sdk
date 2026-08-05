use std::time::Duration;

use eyeball_im::VectorDiff;
use imbl::Vector;
use matrix_sdk::{
    event_cache::TimelineVectorDiffs,
    linked_chunk::{LinkedChunkId, lazy_loader::from_all_chunks},
    test_utils::mocks::MatrixMockServer,
    timeout::timeout,
};
use matrix_sdk_base::event_cache::Event;
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{event_id, room_id, user_id};
use tokio::sync::broadcast;

/// Wait for the pinned-events background task to reload from the network,
/// applying the diffs to `events` until it stabilizes.
async fn wait_for_load(
    events: &mut Vector<Event>,
    subscriber: &mut broadcast::Receiver<TimelineVectorDiffs>,
) {
    if !events.is_empty() {
        return;
    }
    while let Ok(Ok(up)) = timeout(subscriber.recv(), Duration::from_millis(300)).await {
        for diff in up.diffs {
            diff.apply(events);
        }
        if !events.is_empty() {
            break;
        }
    }
}

#[async_test]
async fn test_ignored_user_removes_pinned_events_and_filters_reload() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let dexter = user_id!("@dexter:lab.org");
    let pinned_event_id = event_id!("$pinned_event");

    let room_id_a = room_id!("!omelette:fromage.fr");
    let room_id_b = room_id!("!galette:saucisse.bzh");

    let f = EventFactory::new().sender(dexter);
    let pinned_event = f.text_msg("I'm pinned!").event_id(pinned_event_id).into_event();
    server.mock_room_event().match_event_id().ok(pinned_event).mount().await;

    for room_id in [&room_id_a, &room_id_b] {
        let pinned_events_state = f.room_pinned_events(vec![pinned_event_id.to_owned()]);
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_state_bulk(vec![pinned_events_state.into()]),
            )
            .await;
    }

    // Given a pinned-events cache for room A containing dexter's event,
    let (pinned_cache, _drop_handles) =
        client.event_cache().pinned_events(room_id_a).await.unwrap();
    let (events, mut sub_a) = pinned_cache.subscribe().await.unwrap();
    let mut events: Vector<Event> = events.into();
    wait_for_load(&mut events, &mut sub_a).await;

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_id(), Some(pinned_event_id));

    // When `dexter` is ignored,
    server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_global_account_data(f.ignored_user_list([dexter.to_owned()]));
        })
        .await;

    // Then his pinned event is removed from the in-memory cache,
    {
        let mut removed = false;
        while let Ok(Ok(up)) = timeout(sub_a.recv(), Duration::from_millis(300)).await {
            for diff in &up.diffs {
                if matches!(diff, VectorDiff::Remove { .. }) {
                    removed = true;
                }
            }
            for diff in up.diffs {
                diff.apply(&mut events);
            }
            if removed && events.is_empty() {
                break;
            }
        }

        assert!(removed, "an event should have been removed");
        assert!(events.is_empty(), "pinned events should be empty after ignoring dexter");
    }

    // And removed from the persistent store, so it doesn't resurface after a
    // restart.
    {
        let store_lock = client.event_cache_store().lock().await.unwrap();
        let store = store_lock.as_clean().unwrap();
        let all_chunks =
            store.load_all_chunks(LinkedChunkId::PinnedEvents(room_id_a)).await.unwrap();
        let linked_chunk = from_all_chunks::<128, _, _>(all_chunks).unwrap().unwrap();
        let events: Vec<_> = linked_chunk.items().map(|(_position, event)| event.clone()).collect();
        assert_eq!(events.len(), 0, "pinned events should be removed from the store");
    }

    // A pinned-events cache created after `dexter` was ignored filters his event
    // out during the initial reload (room B).
    let (pinned_cache_b, _drop_handles) =
        client.event_cache().pinned_events(room_id_b).await.unwrap();
    let (events_b, mut sub_b) = pinned_cache_b.subscribe().await.unwrap();
    let mut events_b: Vector<Event> = events_b.into();
    wait_for_load(&mut events_b, &mut sub_b).await;

    assert!(events_b.is_empty(), "pinned events for room B should have been filtered out");
}
