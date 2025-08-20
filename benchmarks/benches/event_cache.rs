use std::{
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use matrix_sdk::{
    RoomInfo, RoomState, SqliteEventCacheStore, StateStore,
    store::StoreConfig,
    sync::{JoinedRoomUpdate, RoomUpdates},
    test_utils::client::MockClientBuilder,
};
use matrix_sdk_base::event_cache::store::{DynEventCacheStore, IntoEventCacheStore, MemoryStore};
use matrix_sdk_test::{ALICE, event_factory::EventFactory};
use ruma::{EventId, RoomId};
use tempfile::tempdir;
use tokio::runtime::Builder;

type StoreBuilder = Box<dyn Fn() -> Pin<Box<dyn Future<Output = Arc<DynEventCacheStore>>>>>;

fn handle_room_updates(c: &mut Criterion) {
    // Create a new asynchronous runtime.
    let runtime = Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .build()
        .expect("Failed to create an asynchronous runtime");

    let mut group = c.benchmark_group("Event cache room updates");
    group.sample_size(10);

    const NUM_EVENTS: usize = 1000;

    for num_rooms in [1, 10, 100] {
        // Add some joined rooms, each with NUM_EVENTS in it, to the sync response.
        let mut room_updates = RoomUpdates::default();

        let mut changes = matrix_sdk::StateChanges::default();

        for i in 0..num_rooms {
            let room_id = RoomId::parse(format!("!room{i}:example.com")).unwrap();
            let event_factory = EventFactory::new().room(&room_id).sender(&ALICE);

            let mut joined_room_update = JoinedRoomUpdate::default();
            for j in 0..NUM_EVENTS {
                let event_id = EventId::parse(format!("$ev{i}_{j}")).unwrap();
                let event =
                    event_factory.text_msg(format!("Message {j}")).event_id(&event_id).into();
                joined_room_update.timeline.events.push(event);
            }
            room_updates.joined.insert(room_id.clone(), joined_room_update);

            changes.add_room(RoomInfo::new(&room_id, RoomState::Joined));
        }

        // Declare new stores for this set of events.
        let temp_dir = Arc::new(tempdir().unwrap());

        let store_builders: Vec<(_, StoreBuilder)> = vec![
            (
                "memory",
                Box::new(|| Box::pin(async { MemoryStore::default().into_event_cache_store() })),
            ),
            (
                "SQLite",
                Box::new(move || {
                    let temp_dir = temp_dir.clone();
                    Box::pin(async move {
                        // Remove all the files in the temp_dir, to reset the event cache state.
                        for entry in temp_dir.path().read_dir().unwrap() {
                            let entry = entry.unwrap();
                            let path = entry.path();
                            if path.is_dir() {
                                // If it's a directory, remove it recursively.
                                std::fs::remove_dir_all(path).unwrap();
                            } else {
                                std::fs::remove_file(path).unwrap();
                            }
                        }

                        // Recreate a new store.
                        SqliteEventCacheStore::open(temp_dir.path().join("bench"), None)
                            .await
                            .unwrap()
                            .into_event_cache_store()
                    })
                }),
            ),
        ];

        let state_store = runtime.block_on(async {
            let state_store = matrix_sdk::MemoryStore::new();
            state_store.save_changes(&changes).await.unwrap();
            Arc::new(state_store)
        });

        for (store_name, store_builder) in &store_builders {
            // Define a state store with all rooms known in it.
            // Define the throughput.
            group.throughput(Throughput::Elements(num_rooms));

            // Bench the handling of room updates.
            group.bench_function(
                BenchmarkId::new(
                    format!("Event cache room updates[{store_name}]"),
                    format!("room count: {num_rooms}"),
                ),
                |bencher| {
                    // Ideally we'd use `iter_with_setup` here, but it doesn't allow an async setup
                    // (which we need to setup the client), see also
                    // https://github.com/bheisler/criterion.rs/issues/751.
                    bencher.to_async(&runtime).iter_custom(|num_iters| {
                        let room_updates = room_updates.clone();
                        let state_store = state_store.clone();

                        async move {
                            let mut total_time = Duration::new(0, 0);

                            for _ in 0..num_iters {
                                // Setup code.
                                let event_cache_store = store_builder().await;

                                let client = MockClientBuilder::new(None)
                                    .on_builder(|builder| {
                                        builder.store_config(
                                            StoreConfig::new(
                                                "cross-process-store-locks-holder-name".to_owned(),
                                            )
                                            .state_store(state_store.clone())
                                            .event_cache_store(event_cache_store.clone()),
                                        )
                                    })
                                    .build()
                                    .await;

                                // Make sure to subscribe to the event cache *after* syncing all
                                // rooms, so that the client knows about all the rooms, but the
                                // event cache doesn't (and need to create the initial empty room
                                // mapping).
                                client.event_cache().subscribe().unwrap();

                                // Run the actual benchmark.
                                let time_before = Instant::now();
                                client
                                    .event_cache()
                                    .handle_room_updates(room_updates.clone())
                                    .await
                                    .unwrap();
                                total_time += time_before.elapsed();
                            }

                            total_time
                        }
                    })
                },
            );
        }
    }

    group.finish()
}

criterion_group! {
    name = event_cache;
    config = Criterion::default();
    targets = handle_room_updates,
}

criterion_main!(event_cache);
