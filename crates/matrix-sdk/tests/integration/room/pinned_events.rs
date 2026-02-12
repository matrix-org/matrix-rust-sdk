use std::{ops::Not as _, sync::Arc};

use matrix_sdk::{
    Room,
    room::IncludeRelations,
    store::StoreConfig,
    test_utils::mocks::{MatrixMockServer, RoomRelationsResponseTemplate},
    timeout::timeout,
};
use matrix_sdk_base::event_cache::store::MemoryStore;
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{EventId, event_id, owned_event_id, room_id, user_id};
use serde_json::json;
use tokio::time::Duration;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path_regex},
};

struct PinningTestSetup<'a> {
    event_id: &'a EventId,
    room_id: &'a ruma::RoomId,
    room: Room,
    client: matrix_sdk::Client,
    server: MatrixMockServer,
}

impl PinningTestSetup<'_> {
    async fn new() -> Self {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a98sd12bjh:example.org");
        let room = server.sync_joined_room(&client, room_id).await;

        server.mock_room_state_encryption().plain().mount().await;

        // This is necessary to get an empty list of pinned events when there are no
        // pinned events state event in the required state.
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.pinned_events/.*"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({})))
            .mount(server.server())
            .await;

        let event_id = event_id!("$a");
        Self { event_id, room_id, client, server, room }
    }

    async fn mock_sync(&mut self, include_pinned_state_event: bool) {
        let f = EventFactory::new().room(self.room_id).sender(user_id!("@a:b.c"));

        let mut joined_room_builder = JoinedRoomBuilder::new(self.room_id)
            .add_timeline_event(f.text_msg("A").event_id(self.event_id).into_raw_sync());

        if include_pinned_state_event {
            let pinned_events =
                f.room_pinned_events(vec![owned_event_id!("$a"), owned_event_id!("$b")]);
            joined_room_builder = joined_room_builder.add_state_bulk(vec![pinned_events.into()]);
        }

        self.server.sync_room(&self.client, joined_room_builder).await;
    }
}

#[async_test]
async fn test_pin_event_is_sent_successfully() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(false).await;

    // Pinning a remote event succeeds.
    setup.server.mock_set_room_pinned_events().ok(owned_event_id!("$42")).mock_once().mount().await;

    assert!(setup.room.pin_event(setup.event_id).await.unwrap());
}

#[async_test]
async fn test_pin_event_is_returning_false_because_is_already_pinned() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(true).await;

    assert!(setup.room.pin_event(setup.event_id).await.unwrap().not());
}

#[async_test]
async fn test_pin_event_is_returning_an_error() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(false).await;

    // Pinning a remote event fails.
    setup.server.mock_set_room_pinned_events().unauthorized().mock_once().mount().await;

    assert!(setup.room.pin_event(setup.event_id).await.is_err());
}

#[async_test]
async fn test_unpin_event_is_sent_successfully() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(true).await;

    // Unpinning a remote event succeeds.
    setup.server.mock_set_room_pinned_events().ok(owned_event_id!("$42")).mock_once().mount().await;

    assert!(setup.room.unpin_event(setup.event_id).await.unwrap());
}

#[async_test]
async fn test_unpin_event_is_returning_false_because_is_not_pinned() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(false).await;

    assert!(setup.room.unpin_event(setup.event_id).await.unwrap().not());
}

#[async_test]
async fn test_unpin_event_is_returning_an_error() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(true).await;

    // Unpinning a remote event fails.
    setup.server.mock_set_room_pinned_events().unauthorized().mock_once().mount().await;

    assert!(setup.room.unpin_event(setup.event_id).await.is_err());
}

#[async_test]
async fn test_pinned_events_are_reloaded_from_storage() {
    let room_id = room_id!("!galette:saucisse.bzh");
    let pinned_event_id = event_id!("$pinned_event");

    let f = EventFactory::new().room(room_id).sender(user_id!("@alice:example.org"));

    // Create the pinned event.
    let pinned_event = f.text_msg("I'm pinned!").event_id(pinned_event_id).into_event();

    // Create an empty event cache store, that we'll populate automatically by
    // fetching the pinned event.
    let event_cache_store = Arc::new(MemoryStore::new());
    let state_store = Arc::new(matrix_sdk::store::MemoryStore::new());

    // Create a first client.
    let server = MatrixMockServer::new().await;

    {
        server
            .mock_room_event()
            .match_event_id()
            .ok(pinned_event.clone())
            .mock_once()
            .mount()
            .await;

        let client = server
            .client_builder()
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("test_store".to_owned())
                        .event_cache_store(event_cache_store.clone())
                        .state_store(state_store.clone()),
                )
            })
            .build()
            .await;

        // Subscribe the event cache to sync updates.
        client.event_cache().subscribe().unwrap();

        // Sync the room with the pinned event ID in the room state.
        //
        // This is important: the pinned events list must include our event ID,
        // otherwise the initial reload from network will clear the storage-loaded
        // events.
        let pinned_events_state = f.room_pinned_events(vec![pinned_event_id.to_owned()]);

        let room = server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_state_bulk(vec![pinned_events_state.into()]),
            )
            .await;

        // Get the room event cache and subscribe to pinned events.
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Subscribe to pinned events - this triggers PinnedEventCache::new() which
        // spawns a task that calls reload_from_storage() first.
        let (events, mut subscriber) = room_event_cache.subscribe_to_pinned_events().await.unwrap();
        let mut events = events.into();

        // Wait for the background task to reload the events.
        while let Ok(Ok(up)) = timeout(subscriber.recv(), Duration::from_millis(300)).await {
            for diff in up.diffs {
                diff.apply(&mut events);
            }
            if !events.is_empty() {
                break;
            }
        }

        // Verify the pinned event was loaded from the network.
        assert_eq!(events.len(), 1, "Expected pinned events to be loaded from network");
        assert_eq!(
            events[0].event_id().unwrap(),
            pinned_event_id,
            "The pinned event should have been loaded from network"
        );
    }

    // Now, create a client reusing the same stores, and observe it reloading
    // the event without performing any network request.
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.store_config(
                StoreConfig::new("test_store".to_owned())
                    .event_cache_store(event_cache_store)
                    .state_store(state_store),
            )
        })
        .build()
        .await;

    // Subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    let room = client.get_room(room_id).unwrap();

    // Get the room event cache and subscribe to pinned events.
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    // Subscribe to pinned events - this triggers PinnedEventCache::new() which
    // spawns a task that calls reload_from_storage() first.
    let (events, mut subscriber) = room_event_cache.subscribe_to_pinned_events().await.unwrap();
    let mut events = events.into();

    // Wait for the background task to reload the events from storage.
    while let Ok(Ok(up)) = timeout(subscriber.recv(), Duration::from_millis(300)).await {
        for diff in up.diffs {
            diff.apply(&mut events);
        }
        if !events.is_empty() {
            break;
        }
    }

    // Verify the pinned event was loaded from the network.
    assert_eq!(events.len(), 1, "Expected pinned events to be reloaded from storage");
    assert_eq!(
        events[0].event_id().unwrap(),
        pinned_event_id,
        "The pinned event should have been reloaded from storage"
    );
}

#[async_test]
async fn test_pinned_events_dont_include_thread_responses() {
    let room_id = room_id!("!galette:saucisse.bzh");
    let pinned_event_id = event_id!("$pinned_event");

    let f = EventFactory::new().room(room_id).sender(user_id!("@alice:example.org"));

    // Create the pinned event, that's a thread root.
    let pinned_event = f.text_msg("I'm pinned!").event_id(pinned_event_id).into_event();

    let thread_response = f
        .text_msg("I'm a thread response!")
        .in_thread(pinned_event_id, pinned_event_id)
        .into_event();

    // Create a first client.
    let server = MatrixMockServer::new().await;

    server.mock_room_event().match_event_id().ok(pinned_event.clone()).mock_once().mount().await;

    // Serve a thread relation over network; it should NOT be included in the pinned
    // event cache for that room.
    server
        .mock_room_relations()
        .match_subrequest(IncludeRelations::AllRelations)
        .match_target_event(pinned_event_id.to_owned())
        .ok(RoomRelationsResponseTemplate::default()
            .events(vec![thread_response.raw().clone().cast_unchecked()]))
        .mock_once()
        .mount()
        .await;

    let client = server.client_builder().build().await;

    // Subscribe the event cache to sync updates.
    client.event_cache().subscribe().unwrap();

    // Sync the room with the pinned event ID in the room state.
    //
    // This is important: the pinned events list must include our event ID,
    // otherwise the initial reload from network will clear the storage-loaded
    // events.
    let pinned_events_state = f.room_pinned_events(vec![pinned_event_id.to_owned()]);

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![pinned_events_state.into()]),
        )
        .await;

    // Get the room event cache and subscribe to pinned events.
    let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

    // Subscribe to pinned events - this triggers PinnedEventCache::new() which
    // spawns a task that calls reload_from_storage() first.
    let (events, mut subscriber) = room_event_cache.subscribe_to_pinned_events().await.unwrap();
    let mut events = events.into();

    // Wait for the background task to reload the events.
    while let Ok(Ok(up)) = timeout(subscriber.recv(), Duration::from_millis(300)).await {
        for diff in up.diffs {
            diff.apply(&mut events);
        }
        if !events.is_empty() {
            break;
        }
    }

    // Verify the pinned event was loaded from the network, and that there wasn't
    // any other event loaded (in particular, the thread response shouldn't be
    // included in the pinned events).
    assert_eq!(events.len(), 1, "Expected pinned events to be loaded from network");
    assert_eq!(
        events[0].event_id().unwrap(),
        pinned_event_id,
        "The pinned event should have been loaded from network"
    );
}
