use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use matrix_sdk::{
    SqliteEventCacheStore,
    store::StoreConfig,
    sync::{JoinedRoomUpdate, RoomUpdates},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::event_cache::store::{IntoEventCacheStore, MemoryStore};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, event_factory::EventFactory};
use ruma::{EventId, RoomId};
use tempfile::tempdir;
use tokio::runtime::Builder;

fn handle_room_updates(c: &mut Criterion) {
    // Create a new asynchronous runtime.
    let runtime = Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .build()
        .expect("Failed to create an asynchronous runtime");

    let mut group = c.benchmark_group("reading");
    group.sample_size(10);

    const NUM_EVENTS: usize = 1000;

    for num_rooms in [1, 10, 100] {
        // Add some joined rooms, each with NUM_EVENTS in it, to the sync response.
        let mut room_updates = RoomUpdates::default();

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
        }

        // Declare new stores for this set of events.
        let sqlite_temp_dir = tempdir().unwrap();
        let stores = vec![
            ("memory store", MemoryStore::default().into_event_cache_store()),
            (
                "sqlite store",
                runtime.block_on(async {
                    SqliteEventCacheStore::open(sqlite_temp_dir.path().join("bench"), None)
                        .await
                        .unwrap()
                        .into_event_cache_store()
                }),
            ),
        ];

        let server = Arc::new(runtime.block_on(MatrixMockServer::new()));

        for (store_name, store) in &stores {
            // Define the throughput.
            group.throughput(Throughput::Elements(num_rooms));

            // Bench the handling of room updates.
            group.bench_function(
                BenchmarkId::new(format!("handle_room_updates/{store_name}"), num_rooms),
                |bencher| {
                    // Ideally we'd use `iter_with_setup` here, but it doesn't allow an async setup
                    // (which we need to setup the client), see also
                    // https://github.com/bheisler/criterion.rs/issues/751.
                    bencher.to_async(&runtime).iter_custom(|num_iters| {
                        let room_updates = room_updates.clone();
                        let server = server.clone();

                        async move {
                            let mut total_time = Duration::new(0, 0);

                            for _ in 0..num_iters {
                                // Setup code.
                                let client = server
                                    .client_builder()
                                    .store_config(
                                        StoreConfig::new(
                                            "cross-process-store-locks-holder-name".to_owned(),
                                        )
                                        .event_cache_store(store.clone()),
                                    )
                                    .build()
                                    .await;

                                client.event_cache().subscribe().unwrap();

                                // Make sure the client knows about all the to-be joined rooms.
                                server
                                    .mock_sync()
                                    .ok_and_run(&client, |builder| {
                                        for room_id in room_updates.joined.keys() {
                                            builder
                                                .add_joined_room(JoinedRoomBuilder::new(room_id));
                                        }
                                    })
                                    .await;

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

        for (_store_name, store) in stores.into_iter() {
            let _guard = runtime.enter();
            drop(store);
        }
    }

    group.finish()
}

fn criterion() -> Criterion {
    let criterion = Criterion::default();

    criterion
}

criterion_group! {
    name = event_cache;
    config = criterion();
    targets = handle_room_updates,
}

criterion_main!(event_cache);
