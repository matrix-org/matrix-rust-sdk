use std::{ops::Deref, sync::Arc};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use matrix_sdk_crypto::{EncryptionSettings, OlmMachine};
use matrix_sdk_sqlite::SqliteCryptoStore;
use matrix_sdk_test::ruma_response_from_json;
use ruma::{
    DeviceId, OwnedUserId, TransactionId, UserId,
    api::client::{
        keys::{claim_keys, get_keys},
        to_device::send_event_to_device::v3::Response as ToDeviceResponse,
    },
    device_id, room_id, user_id,
};
use serde_json::Value;
use tokio::runtime::Builder;

fn alice_id() -> &'static UserId {
    user_id!("@alice:example.org")
}

fn alice_device_id() -> &'static DeviceId {
    device_id!("JLAFKJWSCS")
}

fn keys_query_response() -> get_keys::v3::Response {
    let data = include_bytes!("crypto_bench/keys_query.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    ruma_response_from_json(&data)
}

fn keys_claim_response() -> claim_keys::v3::Response {
    let data = include_bytes!("crypto_bench/keys_claim.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    ruma_response_from_json(&data)
}

fn huge_keys_query_response() -> get_keys::v3::Response {
    let data = include_bytes!("crypto_bench/keys_query_2000_members.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    ruma_response_from_json(&data)
}

pub fn keys_query(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");
    let machine = runtime.block_on(OlmMachine::new(alice_id(), alice_device_id()));
    let response = keys_query_response();
    let txn_id = TransactionId::new();

    let count = response.device_keys.values().fold(0, |acc, d| acc + d.len())
        + response.master_keys.len()
        + response.self_signing_keys.len()
        + response.user_signing_keys.len();

    let mut group = c.benchmark_group("Keys querying");
    group.throughput(Throughput::Elements(count as u64));

    let name = format!("{count} device and cross signing keys");

    // Benchmark memory store.

    group.bench_with_input(
        BenchmarkId::new("Device keys query [memory]", &name),
        &response,
        |b, response| {
            b.to_async(&runtime)
                .iter(|| async { machine.mark_request_as_sent(&txn_id, response).await.unwrap() })
        },
    );

    // Benchmark sqlite store.

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(runtime.block_on(SqliteCryptoStore::open(dir.path(), None)).unwrap());
    let machine = runtime
        .block_on(OlmMachine::with_store(alice_id(), alice_device_id(), store, None))
        .unwrap();

    group.bench_with_input(
        BenchmarkId::new("Device keys query [SQLite]", &name),
        &response,
        |b, response| {
            b.to_async(&runtime)
                .iter(|| async { machine.mark_request_as_sent(&txn_id, response).await.unwrap() })
        },
    );

    {
        let _guard = runtime.enter();
        drop(machine);
    }

    group.finish()
}

/// This test panics on the CI, not sure why so we're disabling it for now.
#[cfg(not(feature = "codspeed"))]
pub fn keys_claiming(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");

    let keys_query_response = keys_query_response();
    let txn_id = TransactionId::new();

    let response = keys_claim_response();

    let count = response.one_time_keys.values().fold(0, |acc, d| acc + d.len());

    let mut group = c.benchmark_group("Olm session creation");
    group.throughput(Throughput::Elements(count as u64));

    let name = format!("{count} one-time keys");

    group.bench_with_input(
        BenchmarkId::new("One-time keys claiming [memory]", &name),
        &response,
        |b, response| {
            b.iter_batched(
                || {
                    let machine = runtime.block_on(OlmMachine::new(alice_id(), alice_device_id()));
                    runtime
                        .block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response))
                        .unwrap();
                    (machine, &runtime, &txn_id)
                },
                move |(machine, runtime, txn_id)| {
                    runtime.block_on(async {
                        machine.mark_request_as_sent(txn_id, response).await.unwrap();
                        drop(machine);
                    })
                },
                criterion::BatchSize::SmallInput,
            )
        },
    );

    group.bench_with_input(
        BenchmarkId::new("One-time keys claiming [SQLite]", &name),
        &response,
        |b, response| {
            b.iter_batched(
                || {
                    let dir = tempfile::tempdir().unwrap();
                    let store = Arc::new(
                        runtime.block_on(SqliteCryptoStore::open(dir.path(), None)).unwrap(),
                    );

                    let machine = runtime
                        .block_on(OlmMachine::with_store(
                            alice_id(),
                            alice_device_id(),
                            store,
                            None,
                        ))
                        .unwrap();
                    runtime
                        .block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response))
                        .unwrap();
                    (machine, &runtime, &txn_id)
                },
                move |(machine, runtime, txn_id)| {
                    runtime.block_on(async {
                        machine.mark_request_as_sent(txn_id, response).await.unwrap();
                    });

                    let _ = runtime.enter();
                    drop(machine);
                },
                criterion::BatchSize::SmallInput,
            )
        },
    );

    group.finish()
}

pub fn room_key_sharing(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");

    let keys_query_response = keys_query_response();
    let txn_id = TransactionId::new();
    let response = keys_claim_response();
    let room_id = room_id!("!test:localhost");

    let to_device_response = ToDeviceResponse::new();
    let users: Vec<OwnedUserId> = keys_query_response.device_keys.keys().cloned().collect();

    let count = response.one_time_keys.values().fold(0, |acc, d| acc + d.len());

    let machine = runtime.block_on(OlmMachine::new(alice_id(), alice_device_id()));
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response)).unwrap();
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    let mut group = c.benchmark_group("Room key sharing");
    group.throughput(Throughput::Elements(count as u64));
    let name = format!("{count} devices");

    // Benchmark memory store.

    group.bench_function(BenchmarkId::new("Room key sharing [memory]", &name), |b| {
        b.to_async(&runtime).iter(|| async {
            let requests = machine
                .share_room_key(
                    room_id,
                    users.iter().map(Deref::deref),
                    EncryptionSettings::default(),
                )
                .await
                .unwrap();

            assert!(!requests.is_empty());

            for request in requests {
                machine.mark_request_as_sent(&request.txn_id, &to_device_response).await.unwrap();
            }

            machine.discard_room_key(room_id).await.unwrap();
        })
    });

    // Benchmark sqlite store.

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(runtime.block_on(SqliteCryptoStore::open(dir.path(), None)).unwrap());

    let machine = runtime
        .block_on(OlmMachine::with_store(alice_id(), alice_device_id(), store, None))
        .unwrap();
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response)).unwrap();
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    group.bench_function(BenchmarkId::new("Room key sharing [SQLite]", &name), |b| {
        b.to_async(&runtime).iter(|| async {
            let requests = machine
                .share_room_key(
                    room_id,
                    users.iter().map(Deref::deref),
                    EncryptionSettings::default(),
                )
                .await
                .unwrap();

            assert!(!requests.is_empty());

            for request in requests {
                machine.mark_request_as_sent(&request.txn_id, &to_device_response).await.unwrap();
            }

            machine.discard_room_key(room_id).await.unwrap();
        })
    });

    {
        let _guard = runtime.enter();
        drop(machine);
    }

    group.finish()
}

pub fn devices_missing_sessions_collecting(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");

    let machine = runtime.block_on(OlmMachine::new(alice_id(), alice_device_id()));
    let response = huge_keys_query_response();
    let txn_id = TransactionId::new();
    let users: Vec<OwnedUserId> = response.device_keys.keys().cloned().collect();

    let count = response.device_keys.values().fold(0, |acc, d| acc + d.len());

    let mut group = c.benchmark_group("Devices missing sessions collecting");
    group.throughput(Throughput::Elements(count as u64));

    let name = format!("{count} devices");

    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    // Benchmark memory store.

    group.bench_function(BenchmarkId::new("Devices collecting [memory]", &name), |b| {
        b.to_async(&runtime).iter_with_large_drop(|| async {
            machine.get_missing_sessions(users.iter().map(Deref::deref)).await.unwrap()
        })
    });

    // Benchmark sqlite store.

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(runtime.block_on(SqliteCryptoStore::open(dir.path(), None)).unwrap());

    let machine = runtime
        .block_on(OlmMachine::with_store(alice_id(), alice_device_id(), store, None))
        .unwrap();

    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    group.bench_function(BenchmarkId::new("Devices collecting [SQLite]", &name), |b| {
        b.to_async(&runtime).iter(|| async {
            machine.get_missing_sessions(users.iter().map(Deref::deref)).await.unwrap()
        })
    });

    {
        let _guard = runtime.enter();
        drop(machine);
    }

    group.finish()
}

#[cfg(not(feature = "codspeed"))]
criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = keys_query, keys_claiming, room_key_sharing, devices_missing_sessions_collecting,
}

#[cfg(feature = "codspeed")]
criterion_group! {
    name = benches;
    config = Criterion::default();
    targets = keys_query, room_key_sharing, devices_missing_sessions_collecting,
}

criterion_main!(benches);
