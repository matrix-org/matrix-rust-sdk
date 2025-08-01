use std::{sync::Arc, time::Duration};

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use matrix_sdk::{
    SqliteEventCacheStore,
    linked_chunk::{LinkedChunk, LinkedChunkId, Update, lazy_loader},
};
use matrix_sdk_base::event_cache::{
    Event, Gap,
    store::{DEFAULT_CHUNK_CAPACITY, DynEventCacheStore, IntoEventCacheStore, MemoryStore},
};
use matrix_sdk_test::{ALICE, event_factory::EventFactory};
use ruma::{EventId, room_id};
use tempfile::tempdir;
use tokio::runtime::Builder;

#[derive(Clone, Debug)]
enum Operation {
    PushItemsBack(Vec<Event>),
    PushGapBack(Gap),
}

#[cfg(not(feature = "codspeed"))]
const NUMBER_OF_EVENTS: &[u64] = &[10, 100, 1000, 10_000, 100_000];
#[cfg(feature = "codspeed")]
const NUMBER_OF_EVENTS: &[u64] = &[10, 100, 1000];

fn writing(c: &mut Criterion) {
    // Create a new asynchronous runtime.
    let runtime = Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .build()
        .expect("Failed to create an asynchronous runtime");

    let room_id = room_id!("!foo:bar.baz");
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let event_factory = EventFactory::new().room(room_id).sender(&ALICE);

    let mut group = c.benchmark_group("Linked chunk writing");
    group.sample_size(10).measurement_time(Duration::from_secs(30));

    for &number_of_events in NUMBER_OF_EVENTS {
        let sqlite_temp_dir = tempdir().unwrap();

        // Declare new stores for this set of events.
        let stores: [(&str, Option<Arc<DynEventCacheStore>>); 3] = [
            ("none", None),
            ("memory store", Some(MemoryStore::default().into_event_cache_store())),
            (
                "sqlite store",
                runtime.block_on(async {
                    Some(
                        SqliteEventCacheStore::open(sqlite_temp_dir.path().join("bench"), None)
                            .await
                            .unwrap()
                            .into_event_cache_store(),
                    )
                }),
            ),
        ];

        for (store_name, store) in stores {
            // Create the operations we want to bench.
            let mut operations = Vec::new();

            {
                let mut events = (0..number_of_events)
                    .map(|nth| {
                        event_factory
                            .text_msg("foo")
                            .event_id(&EventId::parse(format!("$ev{nth}")).unwrap())
                            .into_event()
                    })
                    .peekable();

                let mut gap_nth = 0;

                while events.peek().is_some() {
                    {
                        let events_to_push_back = events.by_ref().take(80).collect::<Vec<_>>();

                        if events_to_push_back.is_empty() {
                            break;
                        }

                        operations.push(Operation::PushItemsBack(events_to_push_back));
                    }

                    {
                        operations.push(Operation::PushGapBack(Gap {
                            prev_token: format!("gap{gap_nth}"),
                        }));
                        gap_nth += 1;
                    }
                }
            }

            // Define the throughput.
            group.throughput(Throughput::Elements(number_of_events));

            // Get a bencher.
            group.bench_with_input(
                BenchmarkId::new(format!("Linked chunk writing [{store_name}]"), number_of_events),
                &operations,
                |bencher, operations| {
                    // Bench the routine.
                    bencher.to_async(&runtime).iter_batched(
                        || operations.clone(),
                        |operations| async {
                            // The routine to bench!

                            let mut linked_chunk = LinkedChunk::<DEFAULT_CHUNK_CAPACITY, Event, Gap>::new_with_update_history();

                            for operation in operations {
                                match operation {
                                    Operation::PushItemsBack(events) => linked_chunk.push_items_back(events),
                                    Operation::PushGapBack(gap) => linked_chunk.push_gap_back(gap),
                                }
                            }

                            if let Some(store) = &store {
                                let updates = linked_chunk.updates().unwrap().take();
                                store.handle_linked_chunk_updates(linked_chunk_id, updates).await.unwrap();
                                // Empty the store.
                                store.handle_linked_chunk_updates(linked_chunk_id, vec![Update::Clear]).await.unwrap();
                            }

                        },
                        BatchSize::SmallInput
                    )
                },
            );

            {
                let _guard = runtime.enter();
                drop(store);
            }
        }
    }

    group.finish()
}

fn reading(c: &mut Criterion) {
    // Create a new asynchronous runtime.
    let runtime = Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .build()
        .expect("Failed to create an asynchronous runtime");

    let room_id = room_id!("!foo:bar.baz");
    let linked_chunk_id = LinkedChunkId::Room(room_id);
    let event_factory = EventFactory::new().room(room_id).sender(&ALICE);

    let mut group = c.benchmark_group("Linked chunk reading");
    group.sample_size(10);

    for &num_events in NUMBER_OF_EVENTS {
        let sqlite_temp_dir = tempdir().unwrap();

        // Declare new stores for this set of events.
        let stores: [(&str, Arc<DynEventCacheStore>); 2] = [
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

        for (store_name, store) in stores {
            // Store some events and gap chunks in the store.
            {
                let mut events = (0..num_events)
                    .map(|nth| {
                        event_factory
                            .text_msg("foo")
                            .event_id(&EventId::parse(format!("$ev{nth}")).unwrap())
                            .into_event()
                    })
                    .peekable();

                let mut lc =
                    LinkedChunk::<DEFAULT_CHUNK_CAPACITY, Event, Gap>::new_with_update_history();
                let mut num_gaps = 0;

                while events.peek().is_some() {
                    let events_chunk = events.by_ref().take(80).collect::<Vec<_>>();

                    if events_chunk.is_empty() {
                        break;
                    }

                    lc.push_items_back(events_chunk);
                    lc.push_gap_back(Gap { prev_token: format!("gap{num_gaps}") });

                    num_gaps += 1;
                }

                // Now persist the updates to recreate this full linked chunk.
                let updates = lc.updates().unwrap().take();
                runtime
                    .block_on(store.handle_linked_chunk_updates(linked_chunk_id, updates))
                    .unwrap();
            }

            // Define the throughput.
            group.throughput(Throughput::Elements(num_events));

            // Bench the lazy loader.
            group.bench_function(
                BenchmarkId::new(format!("Linked chunk lazy loader[{store_name}]"), num_events),
                |bencher| {
                    // Bench the routine.
                    bencher.to_async(&runtime).iter(|| async {
                        // Load the last chunk first,
                        let (last_chunk, chunk_id_gen) =
                            store.load_last_chunk(linked_chunk_id).await.unwrap();

                        let mut lc =
                            lazy_loader::from_last_chunk::<128, _, _>(last_chunk, chunk_id_gen)
                                .expect("no error when reconstructing the linked chunk")
                                .expect("there is a linked chunk in the store");

                        // Then load until the start of the linked chunk.
                        let mut cur_chunk_id = lc.chunks().next().unwrap().identifier();
                        while let Some(prev) =
                            store.load_previous_chunk(linked_chunk_id, cur_chunk_id).await.unwrap()
                        {
                            cur_chunk_id = prev.identifier;
                            lazy_loader::insert_new_first_chunk(&mut lc, prev)
                                .expect("no error when linking the previous lazy-loaded chunk");
                        }
                    })
                },
            );

            // Bench the metadata loader.
            group.bench_function(
                BenchmarkId::new(format!("Linked chunk metadata loader[{store_name}]"), num_events),
                |bencher| {
                    // Bench the routine.
                    bencher.to_async(&runtime).iter(|| async {
                        let _metadata = store
                            .load_all_chunks_metadata(linked_chunk_id)
                            .await
                            .expect("metadata must load");
                    })
                },
            );

            {
                let _guard = runtime.enter();
                drop(store);
            }
        }
    }

    group.finish()
}

criterion_group! {
    name = event_cache;
    config = Criterion::default();
    targets = writing, reading,
}

criterion_main!(event_cache);
