use std::{sync::Arc, time::Duration};

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use matrix_sdk::{
    linked_chunk::{LinkedChunk, Update},
    SqliteEventCacheStore,
};
use matrix_sdk_base::event_cache::{
    store::{DynEventCacheStore, IntoEventCacheStore, MemoryStore, DEFAULT_CHUNK_CAPACITY},
    Event, Gap,
};
use matrix_sdk_test::{event_factory::EventFactory, ALICE};
use ruma::{room_id, EventId};
use tempfile::tempdir;
use tokio::runtime::Builder;

#[derive(Clone, Debug)]
enum Operation {
    PushItemsBack(Vec<Event>),
    PushGapBack(Gap),
}

pub fn writing(c: &mut Criterion) {
    // Create a new asynchronous runtime.
    let runtime = Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .build()
        .expect("Failed to create an asynchronous runtime");

    let room_id = room_id!("!foo:bar.baz");
    let event_factory = EventFactory::new().room(room_id).sender(&ALICE);

    let mut group = c.benchmark_group("writing");
    group.sample_size(10).measurement_time(Duration::from_secs(30));

    for number_of_events in [10, 100, 1000, 10_000, 100_000] {
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
                BenchmarkId::new(store_name, number_of_events),
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
                                store.handle_linked_chunk_updates(room_id, updates).await.unwrap();
                                // Empty the store.
                                store.handle_linked_chunk_updates(room_id, vec![Update::Clear]).await.unwrap();
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

fn criterion() -> Criterion {
    #[cfg(target_os = "linux")]
    let criterion = Criterion::default().with_profiler(pprof::criterion::PProfProfiler::new(
        100,
        pprof::criterion::Output::Flamegraph(None),
    ));
    #[cfg(not(target_os = "linux"))]
    let criterion = Criterion::default();

    criterion
}

criterion_group! {
    name = event_cache;
    config = criterion();
    targets = writing,
}
criterion_main!(event_cache);
