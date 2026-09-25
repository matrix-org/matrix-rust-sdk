use std::{collections::BTreeMap, sync::Arc};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use matrix_sdk::{
    Client, RoomInfo, RoomState, SessionTokens, StateChanges,
    authentication::matrix::MatrixSession, config::StoreConfig,
    cross_process_lock::CrossProcessLockConfig,
};
use matrix_sdk_base::{
    SessionMeta, StateStore as _,
    store::{DynStateStore, IntoStateStore as _, MemoryStore},
};
use matrix_sdk_sqlite::SqliteStateStore;
use matrix_sdk_test::base64_sha256_hash;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId,
    events::receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
    owned_device_id, owned_room_id, owned_user_id, uint,
};
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
        // Synapse's room IDs for rooms v1 to v11 have an 18 characters localpart.
        let raw_room_id = format!("!joinedchamber{i:05}:example.com");

        let room_id = if i % 20 == 19 {
            // Make 1 in 20 rooms use a room v12 ID, which is a base64 hash
            // similar to an event ID.
            RoomId::new_v2(&base64_sha256_hash(raw_room_id.as_bytes())).unwrap()
        } else {
            OwnedRoomId::try_from(raw_room_id).unwrap()
        };
        changes.add_room(RoomInfo::new(&room_id, RoomState::Joined));
    }

    for i in 0..NUM_STRIPPED_JOINED_ROOMS {
        // Synapse's room IDs for rooms v1 to v11 have an 18 characters localpart.
        let raw_room_id = format!("!strippedlodge{i:05}:example.com");

        let room_id = if i % 20 == 19 {
            // Make 1 in 20 rooms use a room v12 ID, which is a base64 hash
            // similar to an event ID.
            RoomId::new_v2(&base64_sha256_hash(raw_room_id.as_bytes())).unwrap()
        } else {
            OwnedRoomId::try_from(raw_room_id).unwrap()
        };
        changes.add_room(RoomInfo::new(&room_id, RoomState::Invited));
    }

    let session = MatrixSession {
        meta: SessionMeta {
            user_id: owned_user_id!("@somebody:example.com"),
            device_id: owned_device_id!("DEVICE_ID"),
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
                    StoreConfig::new(CrossProcessLockConfig::multi_process(
                        "cross-process-store-locks-holder-name",
                    ))
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
                            StoreConfig::new(CrossProcessLockConfig::multi_process(
                                "cross-process-store-locks-holder-name",
                            ))
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

/// Number of events whose read receipts are loaded in the read receipts
/// benchmark.
const NUM_RECEIPT_EVENTS: usize = 500;

/// Number of users with a read receipt in the read receipts benchmark.
const NUM_RECEIPT_USERS: usize = 100;

/// Benchmark loading the read receipts of many events, one event at a time and
/// in one batch, as the timeline does when it adds events.
pub fn load_read_receipts(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().enable_time().build().expect("Can't create runtime");
    let room_id = owned_room_id!("!receipts:example.com");

    let event_ids: Vec<OwnedEventId> = (0..NUM_RECEIPT_EVENTS)
        .map(|i| {
            EventId::new_v2_or_v3(&base64_sha256_hash(format!("${i}").as_bytes()))
                .expect("Invalid event id")
        })
        .collect();
    let event_ids: Vec<&EventId> = event_ids.iter().map(AsRef::as_ref).collect();

    // Every user has read a different event, so most events have no receipts,
    // as in a real room.
    let step = NUM_RECEIPT_EVENTS / NUM_RECEIPT_USERS;
    let content = ReceiptEventContent(
        (0..NUM_RECEIPT_USERS)
            .map(|i| {
                let user_id = OwnedUserId::try_from(format!("@user_{i}:example.com")).unwrap();
                let receipt = Receipt::new(MilliSecondsSinceUnixEpoch(uint!(1)));
                let receipts =
                    BTreeMap::from([(ReceiptType::Read, BTreeMap::from([(user_id, receipt)]))]);

                (event_ids[i * step].to_owned(), receipts)
            })
            .collect(),
    );

    let mut changes = StateChanges::default();
    changes.add_receipts(&room_id, content);

    let memory_store = MemoryStore::new();
    runtime.block_on(memory_store.save_changes(&changes)).expect("initial filling of mem failed");

    let mut stores: Vec<(&str, Arc<DynStateStore>)> =
        vec![("memory store", memory_store.into_state_store())];
    let mut sqlite_dirs = Vec::new();

    for (label, encryption_password) in
        [("SQLite, clear", None), ("SQLite, encrypted", Some("hunter2"))]
    {
        let sqlite_dir = tempfile::tempdir().unwrap();
        let sqlite_store = runtime
            .block_on(SqliteStateStore::open(sqlite_dir.path(), encryption_password))
            .unwrap();
        runtime
            .block_on(sqlite_store.save_changes(&changes))
            .expect("initial filling of sqlite failed");

        stores.push((label, sqlite_store.into_state_store()));
        sqlite_dirs.push(sqlite_dir);
    }

    let mut group = c.benchmark_group("Load read receipts");
    group.throughput(Throughput::Elements(NUM_RECEIPT_EVENTS as u64));

    for (label, store) in &stores {
        group.bench_function(BenchmarkId::new("One event at a time", label), |b| {
            b.to_async(&runtime).iter(|| async {
                for &event_id in &event_ids {
                    store
                        .get_event_room_receipt_events(
                            &room_id,
                            ReceiptType::Read,
                            &ReceiptThread::Unthreaded,
                            event_id,
                        )
                        .await
                        .expect("couldn't load the receipts of an event");
                }
            })
        });

        group.bench_function(BenchmarkId::new("Batch", label), |b| {
            b.to_async(&runtime).iter(|| async {
                let receipts = store
                    .get_event_room_receipt_events_batch(
                        &room_id,
                        ReceiptType::Read,
                        &ReceiptThread::Unthreaded,
                        &event_ids,
                    )
                    .await
                    .expect("couldn't load the receipts of the batch");
                assert_eq!(receipts.len(), NUM_RECEIPT_USERS);
            })
        });
    }

    {
        let _guard = runtime.enter();
        drop(stores);
    }

    group.finish()
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = restore_session, load_read_receipts
}
criterion_main!(benches);
