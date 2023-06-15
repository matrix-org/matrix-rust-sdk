use criterion::*;
use matrix_sdk::{config::StoreConfig, Client, RoomInfo, RoomState, Session, StateChanges};
use matrix_sdk_base::{store::MemoryStore, StateStore as _};
use matrix_sdk_sqlite::SqliteStateStore;
use ruma::{device_id, user_id, RoomId};
use tokio::runtime::Builder;

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

/// Number of joined rooms in the benchmark.
const NUM_JOINED_ROOMS: usize = 10000;

/// Number of stripped rooms in the benchmark.
const NUM_STRIPPED_JOINED_ROOMS: usize = 10000;

pub fn restore_session(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");

    // Create a fake list of changes, and a session to recover from.
    let mut changes = StateChanges::default();

    for i in 0..NUM_JOINED_ROOMS {
        let room_id = RoomId::parse(format!("!room{i}:example.com")).unwrap().to_owned();
        changes.add_room(RoomInfo::new(&room_id, RoomState::Joined));
    }

    for i in 0..NUM_STRIPPED_JOINED_ROOMS {
        let room_id = RoomId::parse(format!("!strippedroom{i}:example.com")).unwrap().to_owned();
        changes.add_room(RoomInfo::new(&room_id, RoomState::Invited));
    }

    let session = Session {
        access_token: "OHEY".to_owned(),
        refresh_token: None,
        user_id: user_id!("@somebody:example.com").to_owned(),
        device_id: device_id!("DEVICE_ID").to_owned(),
    };

    // Start the benchmark.

    let mut group = c.benchmark_group("Client reload");
    group.throughput(Throughput::Elements(100));

    const NAME: &str = "restore a session";

    // Memory
    let mem_store = MemoryStore::new();
    runtime.block_on(mem_store.save_changes(&changes)).expect("initial filling of mem failed");

    group.bench_with_input(BenchmarkId::new("memory store", NAME), &mem_store, |b, store| {
        b.to_async(&runtime).iter(|| async {
            let client = Client::builder()
                .homeserver_url("https://matrix.example.com")
                .store_config(StoreConfig::new().state_store(store.clone()))
                .build()
                .await
                .expect("Can't build client");
            client.restore_session(session.clone()).await.expect("couldn't restore session");
        })
    });

    for encryption_password in [None, Some("hunter2")] {
        let encrypted_suffix = if encryption_password.is_some() { "encrypted" } else { "clear" };

        // Sqlite
        let sqlite_dir = tempfile::tempdir().unwrap();
        let sqlite_store = runtime
            .block_on(SqliteStateStore::open(sqlite_dir.path(), encryption_password))
            .unwrap();
        runtime
            .block_on(sqlite_store.save_changes(&changes))
            .expect("initial filling of sqlite failed");

        group.bench_with_input(
            BenchmarkId::new(format!("sqlite store {encrypted_suffix}"), NAME),
            &sqlite_store,
            |b, store| {
                b.to_async(&runtime).iter(|| async {
                    let client = Client::builder()
                        .homeserver_url("https://matrix.example.com")
                        .store_config(StoreConfig::new().state_store(store.clone()))
                        .build()
                        .await
                        .expect("Can't build client");
                    client
                        .restore_session(session.clone())
                        .await
                        .expect("couldn't restore session");
                })
            },
        );

        {
            let _guard = runtime.enter();
            drop(sqlite_store);
        }
    }

    group.finish()
}

criterion_group! {
    name = benches;
    config = criterion();
    targets = restore_session
}
criterion_main!(benches);
