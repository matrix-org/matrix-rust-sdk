use std::time::Duration;

use matrix_sdk::{
    event_cache::EventFocusThreadMode,
    test_utils::mocks::{
        MatrixMockServer, RoomContextResponseTemplate, RoomMessagesResponseTemplate,
    },
};
use matrix_sdk_test::{ALICE, async_test, event_factory::EventFactory};
use ruma::{event_id, room_id};
use serde_json::json;
use tokio::{spawn, time::timeout};
use wiremock::ResponseTemplate;

use super::wait_for_request;

#[async_test]
async fn test_slow_context_does_not_block_other_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!galette:saucisse.fr");
    let other_room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let other_room = server.sync_joined_room(&client, other_room_id).await;
    let (other_room_event_cache, _drop_handles) = other_room.event_cache().await.unwrap();

    // The `/context` request takes a long time.
    let event = f.text_msg("hello").event_id(event_id!("$focused")).into_event();
    server
        .mock_room_event_context()
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "event": event.into_raw().json(),
                    "events_before": [],
                    "events_after": [],
                    "state": [],
                }))
                .set_delay(Duration::from_secs(10)),
        )
        .mount()
        .await;

    let event_cache = client.event_cache().clone();
    let task = spawn(async move {
        event_cache
            .event_focused(room_id, event_id!("$focused"), EventFocusThreadMode::Automatic, 10)
            .await
            .map(|_| ())
    });

    // While the request is in flight, the other room's event cache can be used.
    wait_for_request(&server, "focused").await;
    timeout(Duration::from_secs(2), other_room_event_cache.events())
        .await
        .expect("the other room must not be blocked by the `/context` request")
        .unwrap();

    task.abort();
}

#[async_test]
async fn test_slow_pagination_does_not_block_other_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!galette:saucisse.fr");
    let other_room_id = room_id!("!omelette:fromage.fr");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;
    let other_room = server.sync_joined_room(&client, other_room_id).await;
    let (other_room_event_cache, _drop_handles) = other_room.event_cache().await.unwrap();

    server
        .mock_room_event_context()
        .ok(RoomContextResponseTemplate::new(
            f.text_msg("hello").event_id(event_id!("$focused")).into_event(),
        )
        .start("prev"))
        .mock_once()
        .mount()
        .await;

    let (cache, _drop_handles) = client
        .event_cache()
        .event_focused(room_id, event_id!("$focused"), EventFocusThreadMode::Automatic, 10)
        .await
        .unwrap();

    // The `/messages` request takes a long time.
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default().with_delay(Duration::from_secs(10)))
        .mount()
        .await;

    let task = spawn(async move { cache.paginate_backwards(10).await.map(|_| ()) });

    // While the request is in flight, the other room's event cache can be used.
    wait_for_request(&server, "/messages").await;
    timeout(Duration::from_secs(2), other_room_event_cache.events())
        .await
        .expect("the other room must not be blocked by the `/messages` request")
        .unwrap();

    task.abort();
}

#[async_test]
async fn test_pagination_is_discarded_after_reload() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!galette:saucisse.fr");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    server.sync_joined_room(&client, room_id).await;

    // The initial `/context`, then the one when reloading, with a new token.
    server
        .mock_room_event_context()
        .ok(RoomContextResponseTemplate::new(
            f.text_msg("hello").event_id(event_id!("$focused")).into_event(),
        )
        .start("stale_prev"))
        .mock_once()
        .mount()
        .await;
    server
        .mock_room_event_context()
        .ok(RoomContextResponseTemplate::new(
            f.text_msg("hello").event_id(event_id!("$focused")).into_event(),
        )
        .start("fresh_prev"))
        .mock_once()
        .mount()
        .await;

    let (cache, _drop_handles) = client
        .event_cache()
        .event_focused(room_id, event_id!("$focused"), EventFocusThreadMode::Automatic, 10)
        .await
        .unwrap();

    // The first response arrives after the cache has been reloaded.
    server
        .mock_room_messages()
        .match_from("stale_prev")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("stale").event_id(event_id!("$stale")).into_raw_timeline()])
            .with_delay(Duration::from_secs(2)))
        .mock_once()
        .mount()
        .await;

    // The restarted pagination resolves the gap from the reloaded cache.
    server
        .mock_room_messages()
        .match_from("fresh_prev")
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("old").event_id(event_id!("$old")).into_raw_timeline()]))
        .mock_once()
        .mount()
        .await;

    let task = spawn({
        let cache = cache.clone();
        async move { cache.paginate_backwards(10).await }
    });

    // Reload the cache while the first request is in flight.
    wait_for_request(&server, "/messages").await;
    client.event_cache().clear_all_rooms().await.unwrap();

    let outcome = task.await.unwrap().unwrap();

    // The stale response hasn't been applied.
    let event_ids =
        outcome.events.iter().map(|event| event.event_id().unwrap()).collect::<Vec<_>>();
    assert_eq!(event_ids, [event_id!("$old")]);
    assert!(outcome.hit_end_of_timeline);
    let events = cache.events().await.unwrap();
    let event_ids = events.iter().map(|event| event.event_id().unwrap()).collect::<Vec<_>>();
    assert_eq!(event_ids, [event_id!("$old"), event_id!("$focused")]);
}
