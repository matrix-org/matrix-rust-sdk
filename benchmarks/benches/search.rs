use std::{
    path::Path,
    time::{Duration, Instant},
};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use matrix_sdk::{
    Client, search_index::SearchIndexStoreKind, sleep::sleep, stream::StreamExt as _,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{JoinedRoomBuilder, event_factory::EventFactory};
use ruma::{OwnedRoomId, owned_user_id};
use tempfile::TempDir;
use tokio::runtime::{Builder, Runtime};

const NUMBER_OF_ROOMS: usize = 1000;
const KEY: &str = "key";

fn room_ids(prefix: &str) -> Vec<OwnedRoomId> {
    (0..NUMBER_OF_ROOMS)
        .map(|nth| format!("!{prefix}{nth:04}:example.com").try_into().unwrap())
        .collect()
}

/// A client using an encrypted on-disk search index in `dir`, as the apps do,
/// that joined `rooms`. When `with_messages` is set, each room gets a few
/// messages, one of them matching the search query.
async fn client(
    server: &MatrixMockServer,
    dir: &Path,
    rooms: &[OwnedRoomId],
    with_messages: bool,
) -> Client {
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.search_index_store(SearchIndexStoreKind::EncryptedDirectory(
                dir.to_path_buf(),
                KEY.to_owned(),
            ))
        })
        .build()
        .await;

    let sender = owned_user_id!("@alice:example.com");

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            for room_id in rooms {
                let mut room = JoinedRoomBuilder::new(room_id);

                if with_messages {
                    let f = EventFactory::new().room(room_id).sender(&sender);
                    room = room
                        .add_timeline_event(f.text_msg("good morning").server_ts(1_000))
                        .add_timeline_event(f.text_msg("hello there").server_ts(2_000))
                        .add_timeline_event(f.text_msg("see you later").server_ts(3_000));
                }

                builder.add_joined_room(room);
            }
        })
        .await;

    client
}

/// Time to the first page of a global search, run the way the search service
/// does it.
async fn first_page(client: &Client) {
    let mut stream = Box::pin(client.search_messages("morning".to_owned()).build());
    let page = stream.next().await.unwrap().unwrap();
    assert!(!page.is_empty());
}

/// Wait until the indexing task has indexed the matching message of every
/// room.
async fn wait_for_indexing(client: &Client) {
    loop {
        let mut count = 0;
        let mut stream = Box::pin(client.search_messages("morning".to_owned()).build());
        while let Some(page) = stream.next().await {
            count += page.unwrap().len();
        }

        if count == NUMBER_OF_ROOMS {
            return;
        }

        sleep(Duration::from_millis(500)).await;
    }
}

/// Populate an on-disk search index for every room, then drop the client so
/// later clients open the indexes from disk.
fn populate(runtime: &Runtime, server: &MatrixMockServer, rooms: &[OwnedRoomId]) -> TempDir {
    let dir = TempDir::new().unwrap();

    runtime.block_on(async {
        let client = client(server, dir.path(), rooms, true).await;
        wait_for_indexing(&client).await;
    });

    dir
}

pub fn search(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().enable_all().build().expect("Can't create runtime");
    let server = runtime.block_on(MatrixMockServer::new());

    let indexed_rooms = room_ids("indexed");
    let indexed_dir = populate(&runtime, &server, &indexed_rooms);

    let mut group = c.benchmark_group("Search");
    group.sample_size(10);

    // Every room index is opened from disk (key derivation + tantivy open).
    group.bench_function(BenchmarkId::new("First page", "cold"), |bencher| {
        bencher.iter_custom(|iters| {
            timed(iters, || {
                let client =
                    runtime.block_on(client(&server, indexed_dir.path(), &indexed_rooms, false));
                let start = Instant::now();
                runtime.block_on(first_page(&client));
                start.elapsed()
            })
        })
    });

    // Every room index is already open: pure query and cross-room merge cost.
    let warm_client = runtime.block_on(async {
        let client = client(&server, indexed_dir.path(), &indexed_rooms, false).await;
        first_page(&client).await;
        client
    });

    group.bench_function(BenchmarkId::new("First page", "warm"), |bencher| {
        bencher.to_async(&runtime).iter(|| first_page(&warm_client))
    });

    // Searching also covers rooms that have no index at all.
    let no_index_rooms = room_ids("noindex");
    let mut mixed_rooms = indexed_rooms.clone();
    mixed_rooms.extend(no_index_rooms);

    group.bench_function(
        BenchmarkId::new("First page", "cold, plus rooms without index"),
        |bencher| {
            bencher.iter_custom(|iters| {
                timed(iters, || {
                    let client =
                        runtime.block_on(client(&server, indexed_dir.path(), &mixed_rooms, false));
                    let start = Instant::now();
                    runtime.block_on(first_page(&client));
                    start.elapsed()
                })
            })
        },
    );

    group.finish();

    let _guard = runtime.enter();
    drop(warm_client);
    drop(server);
}

/// Sum the durations of `iters` runs of `run`, which times only the part it
/// cares about.
fn timed(iters: u64, mut run: impl FnMut() -> Duration) -> Duration {
    (0..iters).map(|_| run()).sum()
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = search
}
criterion_main!(benches);
