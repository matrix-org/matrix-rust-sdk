use std::{pin::Pin, sync::Arc};

use assert_matches::assert_matches;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use matrix_sdk::{
    RoomInfo, RoomState, SqliteEventCacheStore, StateStore,
    cross_process_lock::CrossProcessLockConfig,
    latest_events::{LatestEventValue, bench_new_remote},
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
    store::StoreConfig,
    sync::{JoinedRoomUpdate, RoomUpdates},
    test_utils::client::MockClientBuilder,
};
use matrix_sdk_base::event_cache::store::{DynEventCacheStore, IntoEventCacheStore, MemoryStore};
use matrix_sdk_test::{User, base64_sha256_hash, event_factory::EventFactory};
use ruma::{
    OwnedRoomId, RoomId, event_id,
    events::{relation::RelationType, room::message::RoomMessageEventContentWithoutRelation},
    room_id, user_id,
};
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
        // Add some joined rooms, each with NUM_EVENTS in it, to the sync
        // response.
        let mut room_updates = RoomUpdates::default();

        let mut changes = matrix_sdk::StateChanges::default();

        for i in 0..num_rooms {
            // Synapse's room IDs for rooms v1 to v11 have an 18 characters
            // localpart.
            let raw_room_id = format!("!firstbatchroom{i:04}:example.com");

            let room_id = if i % 10 == 9 {
                // Make 1 in 10 rooms use a room v12 ID, which is a base64 hash
                // similar to an event ID.
                RoomId::new_v2(&base64_sha256_hash(raw_room_id.as_bytes())).unwrap()
            } else {
                OwnedRoomId::try_from(raw_room_id).unwrap()
            };
            let event_factory = EventFactory::new().room(&room_id).sender(User::Alice);

            let mut joined_room_update = JoinedRoomUpdate::default();
            for j in 0..NUM_EVENTS {
                let event = event_factory.text_msg(format!("Message {j}")).into();
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
                        // Remove all the files in the temp_dir, to reset the
                        // event cache state.
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
            let client = runtime.block_on(async {
                let event_cache_store = store_builder().await;

                MockClientBuilder::new(None)
                    .on_builder(|builder| {
                        builder.store_config(
                            StoreConfig::new(CrossProcessLockConfig::multi_process(
                                "cross-process-store-locks-holder-name",
                            ))
                            .state_store(state_store.clone())
                            .event_cache_store(event_cache_store),
                        )
                    })
                    .build()
                    .await
            });

            // Define a state store with all rooms known in it. Define the
            // throughput.
            group.throughput(Throughput::Elements(num_rooms));

            // Bench the handling of room updates.
            group.bench_function(
                BenchmarkId::new(
                    format!("Event cache room updates[{store_name}]"),
                    format!("room count: {num_rooms}"),
                ),
                |bencher| {
                    bencher.to_async(&runtime).iter(
                        // The routine itself.
                        || {
                            let room_updates = room_updates.clone();
                            let client = client.clone();

                            async move {
                                let event_cache = client.event_cache().unwrap();

                                event_cache.clear_all_rooms().await.unwrap();
                                event_cache
                                    .handle_room_updates(room_updates.clone())
                                    .await
                                    .unwrap();
                            }
                        },
                    )
                },
            );
        }
    }

    group.finish()
}

fn find_event_relations(c: &mut Criterion) {
    // Number of other events to saturate the DB, but that will not be affected
    // by the benchmark. A small multiple of this number will be added. When
    // running locally, run with more events than in Codespeed CI.
    #[cfg(feature = "codspeed")]
    const NUM_OTHER_EVENTS: usize = 100;
    #[cfg(not(feature = "codspeed"))]
    const NUM_OTHER_EVENTS: usize = 1000;

    // Create a new asynchronous runtime.
    let runtime = Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .build()
        .expect("Failed to create an asynchronous runtime");

    let mut group = c.benchmark_group("Event cache room updates");
    group.sample_size(10);

    // Room v1-v11 ID.
    let room_id = room_id!("!initialtestingroom:ben.ch");
    // Room v12 ID.
    let other_room_id = room_id!("!ICMDdumUm6RRX_eWYY2wMb2w0CY0Z_5OvlY2gBR6ELc");

    // Make the state store aware of the room, so that `client.get_room()` works
    // with it.
    let mut changes = matrix_sdk::StateChanges::default();
    changes.add_room(RoomInfo::new(room_id, RoomState::Joined));
    changes.add_room(RoomInfo::new(other_room_id, RoomState::Joined));
    let state_store = runtime.block_on(async {
        let state_store = matrix_sdk::MemoryStore::new();
        state_store.save_changes(&changes).await.unwrap();
        Arc::new(state_store)
    });

    for num_related_events in [10, 100, 1000] {
        // Prefill the event cache store with one event and N related events.
        let mut room_updates = RoomUpdates::default();

        let event_factory = EventFactory::new().room(room_id).sender(User::Alice);

        let mut joined_room_update = JoinedRoomUpdate::default();

        // Add the target event.
        let target_event = event_factory.text_msg("hello world").into_event();
        let target_event_id =
            &target_event.event_id().expect("generated event has an event ID").to_owned();
        joined_room_update.timeline.events.push(target_event);

        // Add the numerous edits.
        for i in 0..num_related_events {
            let event = event_factory
                .text_msg(format!("* edit {i}"))
                .edit(
                    target_event_id,
                    RoomMessageEventContentWithoutRelation::text_plain(format!("edit {i}")),
                )
                .into();
            joined_room_update.timeline.events.push(event);
        }

        // Add other events, in the same room, without a relation.
        for i in 0..NUM_OTHER_EVENTS {
            let event = event_factory.text_msg(format!("unrelated message {i}")).into();
            joined_room_update.timeline.events.push(event);
        }

        // Add other events, in the same room, related to other events.
        let other_target_event = event_factory.text_msg("hello world").into_event();
        let other_target_event_id =
            other_target_event.event_id().expect("generated event has an event ID").to_owned();
        joined_room_update.timeline.events.push(other_target_event);

        for _i in 0..NUM_OTHER_EVENTS {
            let event = event_factory.reaction(&other_target_event_id, "👍").into();
            joined_room_update.timeline.events.push(event);
        }

        room_updates.joined.insert(room_id.to_owned(), joined_room_update);

        // Add other events, in another room.
        let mut other_joined_room_update = JoinedRoomUpdate::default();
        let event_factory = event_factory.room(other_room_id);
        for i in 0..NUM_OTHER_EVENTS {
            let event = event_factory.text_msg(format!("hi {i}")).into();
            other_joined_room_update.timeline.events.push(event);
        }
        room_updates.joined.insert(other_room_id.to_owned(), other_joined_room_update);

        changes.add_room(RoomInfo::new(room_id, RoomState::Joined));

        // Declare new stores for this set of events.
        let temp_dir = Arc::new(tempdir().unwrap());

        let stores = vec![
            ("memory", MemoryStore::default().into_event_cache_store()),
            (
                "SQLite",
                runtime.block_on(async {
                    SqliteEventCacheStore::open(temp_dir.path().join("bench"), None)
                        .await
                        .unwrap()
                        .into_event_cache_store()
                }),
            ),
        ];

        for (store_name, event_cache_store) in stores {
            let (client, room_event_cache, _drop_handles) = runtime.block_on(async {
                let client = MockClientBuilder::new(None)
                    .on_builder(|builder| {
                        builder.store_config(
                            StoreConfig::new(CrossProcessLockConfig::multi_process(
                                "cross-process-store-locks-holder-name",
                            ))
                            .state_store(state_store.clone())
                            .event_cache_store(event_cache_store),
                        )
                    })
                    .build()
                    .await;

                let event_cache = client.event_cache().unwrap();

                // Sync the updates before starting the benchmark.
                let mut update_recv = event_cache.subscribe_to_room_generic_updates();

                event_cache.handle_room_updates(room_updates.clone()).await.unwrap();

                // Wait for the event cache to notify us of the room updates.
                let update = update_recv.recv().await.unwrap();
                assert!(update.room_id == room_id || update.room_id == other_room_id);

                let update = update_recv.recv().await.unwrap();
                assert!(update.room_id == room_id || update.room_id == other_room_id);

                let room_event_cache = event_cache.room(room_id).await.unwrap();

                (client, room_event_cache.0, room_event_cache.1)
            });

            // Define the throughput.
            group.throughput(Throughput::Elements(num_related_events));

            for filter in [None, Some(vec![RelationType::Replacement])] {
                group.bench_function(
                    BenchmarkId::new(
                        format!("Event cache find_event_relations[{store_name}]"),
                        format!(
                            "{num_related_events} events, {} filter",
                            if filter.is_some() { "edits" } else { "#no" },
                        ),
                    ),
                    |bencher| {
                        bencher.to_async(&runtime).iter_batched(
                            // The setup.
                            || (room_event_cache.clone(), filter.clone()),
                            // The routine itself.
                            |(room_event_cache, filter)| async move {
                                let (target, relations) = room_event_cache
                                    .find_event_with_relations(target_event_id, filter)
                                    .await
                                    .unwrap()
                                    .unwrap();
                                assert_eq!(target.event_id().unwrap(), target_event_id);
                                assert_eq!(relations.len(), num_related_events as usize);
                            },
                            criterion::BatchSize::PerIteration,
                        )
                    },
                );
            }

            {
                let _guard = runtime.enter();
                drop(room_event_cache);
                drop(client);
                drop(_drop_handles);
            }
        }
    }

    {
        let _guard = runtime.enter();
        drop(state_store);
    }

    group.finish()
}

fn find_latest_event(c: &mut Criterion) {
    const NUM_EVENTS_PER_CHUNK: usize = 50;

    // Number of stored chunks. Only the last one is loaded in memory.
    const NUM_CHUNKS: u64 = 12;
    const LAST_CHUNK: u64 = NUM_CHUNKS - 1;

    // Create a new asynchronous runtime.
    let runtime = Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .build()
        .expect("Failed to create an asynchronous runtime");

    let mut group = c.benchmark_group("Latest event");
    group.sample_size(10);

    let room_id = room_id!("!latesteventroom:ben.ch");
    let own_user_id = user_id!("@me:ben.ch");
    let event_factory = EventFactory::new().room(room_id).sender(User::Alice);

    // Make the state store aware of the room, so that the event cache can be
    // created for it.
    let mut changes = matrix_sdk::StateChanges::default();
    changes.add_room(RoomInfo::new(room_id, RoomState::Joined));
    let state_store = runtime.block_on(async {
        let state_store = matrix_sdk::MemoryStore::new();
        state_store.save_changes(&changes).await.unwrap();
        Arc::new(state_store)
    });

    // Which chunk holds the only candidate for the latest event.
    let scenarios = [
        ("in memory", LAST_CHUNK),
        ("1st previous chunk", LAST_CHUNK - 1),
        ("10th previous chunk", LAST_CHUNK - 10),
    ];

    for (scenario, candidate_chunk) in scenarios {
        // Fill the chunks with reactions, which can't be the latest event, and
        // put the candidate at the end of `candidate_chunk`.
        let mut updates = Vec::new();

        for chunk in 0..NUM_CHUNKS {
            let mut events: Vec<_> = (0..NUM_EVENTS_PER_CHUNK)
                .map(|_| event_factory.reaction(event_id!("$unknown"), "👍").into_event())
                .collect();

            if chunk == candidate_chunk {
                events.push(event_factory.text_msg("hello world").into_event());
            }

            updates.push(Update::NewItemsChunk {
                previous: chunk.checked_sub(1).map(ChunkIdentifier::new),
                new: ChunkIdentifier::new(chunk),
                next: None,
            });
            updates.push(Update::PushItems {
                at: Position::new(ChunkIdentifier::new(chunk), 0),
                items: events,
            });
        }

        // Declare new stores for this set of events.
        let temp_dir = Arc::new(tempdir().unwrap());

        let stores = vec![
            ("memory", MemoryStore::default().into_event_cache_store()),
            (
                "SQLite",
                runtime.block_on(async {
                    SqliteEventCacheStore::open(temp_dir.path().join("bench"), None)
                        .await
                        .unwrap()
                        .into_event_cache_store()
                }),
            ),
        ];

        for (store_name, event_cache_store) in stores {
            let (client, room_event_cache, _drop_handles) = runtime.block_on(async {
                event_cache_store
                    .handle_linked_chunk_updates(LinkedChunkId::Room(room_id), updates.clone())
                    .await
                    .unwrap();

                let client = MockClientBuilder::new(None)
                    .on_builder(|builder| {
                        builder.store_config(
                            StoreConfig::new(CrossProcessLockConfig::multi_process(
                                "cross-process-store-locks-holder-name",
                            ))
                            .state_store(state_store.clone())
                            .event_cache_store(event_cache_store),
                        )
                    })
                    .build()
                    .await;

                // Only the last chunk is loaded in memory.
                let (room_event_cache, drop_handles) =
                    client.event_cache().unwrap().room(room_id).await.unwrap();

                (client, room_event_cache, drop_handles)
            });

            group.bench_function(
                BenchmarkId::new(format!("Latest event new_remote[{store_name}]"), scenario),
                |bencher| {
                    bencher.to_async(&runtime).iter(|| async {
                        let value = bench_new_remote(&room_event_cache, own_user_id).await;
                        assert_matches!(value, Some(LatestEventValue::Remote(_)));
                    })
                },
            );

            {
                let _guard = runtime.enter();
                drop(room_event_cache);
                drop(client);
                drop(_drop_handles);
            }
        }
    }

    {
        let _guard = runtime.enter();
        drop(state_store);
    }

    group.finish()
}

criterion_group! {
    name = event_cache;
    config = Criterion::default();
    targets = handle_room_updates, find_event_relations, find_latest_event,
}

criterion_main!(event_cache);
