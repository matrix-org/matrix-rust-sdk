use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use matrix_sdk::{
    Client, RoomInfo, RoomState, SessionTokens, StateChanges,
    authentication::matrix::MatrixSession, config::StoreConfig,
};
use matrix_sdk_base::{SessionMeta, StateStore as _, store::MemoryStore};
use matrix_sdk_sqlite::SqliteStateStore;
use ruma::{RoomId, device_id, user_id};
use tokio::runtime::Builder;

/// Number of joined rooms in the benchmark.
const NUM_JOINED_ROOMS: usize = 10000;

/// Number of stripped rooms in the benchmark.
const NUM_STRIPPED_JOINED_ROOMS: usize = 10000;

pub fn restore_session(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().enable_time().build().expect("Can't create runtime");

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

    let session = MatrixSession {
        meta: SessionMeta {
            user_id: user_id!("@somebody:example.com").to_owned(),
            device_id: device_id!("DEVICE_ID").to_owned(),
        },
        tokens: SessionTokens { access_token: "OHEY".to_owned(), refresh_token: None },
    };

    // Start the benchmark.

    let mut group = c.benchmark_group("Client reload");
    group.throughput(Throughput::Elements(100));

    // Memory
    let mem_store = Arc::new(MemoryStore::new());
    runtime.block_on(mem_store.save_changes(&changes)).expect("initial filling of mem failed");

    group.bench_with_input("Restore session [memory store]", &mem_store, |b, store| {
        b.to_async(&runtime).iter(|| async {
            let client = Client::builder()
                .homeserver_url("https://matrix.example.com")
                .store_config(
                    StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                        .state_store(store.clone()),
                )
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
            BenchmarkId::new("Restore session [SQLite]", encrypted_suffix),
            &sqlite_store,
            |b, store| {
                b.to_async(&runtime).iter(|| async {
                    let client = Client::builder()
                        .homeserver_url("https://matrix.example.com")
                        .store_config(
                            StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
                                .state_store(store.clone()),
                        )
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
    config = Criterion::default();
    targets = restore_session
}
criterion_main!(benches);
