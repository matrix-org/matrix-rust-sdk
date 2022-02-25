use std::ops::Deref;

use criterion::*;
use matrix_sdk_crypto::{EncryptionSettings, OlmMachine};
use matrix_sdk_test::response_from_file;
use ruma::{
    api::{
        client::r0::{
            keys::{claim_keys, get_keys},
            to_device::send_event_to_device::Response as ToDeviceResponse,
        },
        IncomingResponse,
    },
    device_id, room_id, user_id, DeviceId, TransactionId, UserId,
};
use serde_json::Value;
use tokio::runtime::Builder;

fn alice_id() -> &'static UserId {
    user_id!("@alice:example.org")
}

fn alice_device_id() -> &'static DeviceId {
    device_id!("JLAFKJWSCS")
}

fn keys_query_response() -> get_keys::Response {
    let data = include_bytes!("./keys_query.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    let data = response_from_file(&data);
    get_keys::Response::try_from_http_response(data).expect("Can't parse the keys upload response")
}

fn keys_claim_response() -> claim_keys::Response {
    let data = include_bytes!("./keys_claim.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    let data = response_from_file(&data);
    claim_keys::Response::try_from_http_response(data)
        .expect("Can't parse the keys upload response")
}

fn huge_keys_query_response() -> get_keys::Response {
    let data = include_bytes!("./keys_query_2000_members.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    let data = response_from_file(&data);
    get_keys::Response::try_from_http_response(data).expect("Can't parse the keys query response")
}

pub fn keys_query(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");
    let machine = OlmMachine::new(alice_id(), alice_device_id());
    let response = keys_query_response();
    let txn_id = TransactionId::new();

    let count = response.device_keys.values().fold(0, |acc, d| acc + d.len())
        + response.master_keys.len()
        + response.self_signing_keys.len()
        + response.user_signing_keys.len();

    let mut group = c.benchmark_group("Keys querying");
    group.throughput(Throughput::Elements(count as u64));

    let name = format!("{} device and cross signing keys", count);

    group.bench_with_input(BenchmarkId::new("memory store", &name), &response, |b, response| {
        b.to_async(&runtime)
            .iter(|| async { machine.mark_request_as_sent(&txn_id, response).await.unwrap() })
    });

    let _dir = tempfile::tempdir().unwrap();
    let machine = OlmMachine::new(
            alice_id(),
            alice_device_id(),
        );

    group.bench_with_input(BenchmarkId::new("sled store", &name), &response, |b, response| {
        b.to_async(&runtime)
            .iter(|| async { machine.mark_request_as_sent(&txn_id, response).await.unwrap() })
    });

    group.finish()
}

pub fn keys_claiming(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");

    let keys_query_response = keys_query_response();
    let txn_id = TransactionId::new();

    let response = keys_claim_response();

    let count = response.one_time_keys.values().fold(0, |acc, d| acc + d.len());

    let mut group = c.benchmark_group("Olm session creation");
    group.throughput(Throughput::Elements(count as u64));

    let name = format!("{} one-time keys", count);

    group.bench_with_input(BenchmarkId::new("memory store", &name), &response, |b, response| {
        b.iter_batched(
            || {
                let machine = OlmMachine::new(alice_id(), alice_device_id());
                runtime
                    .block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response))
                    .unwrap();
                (machine, &runtime, &txn_id)
            },
            move |(machine, runtime, txn_id)| {
                runtime.block_on(machine.mark_request_as_sent(txn_id, response)).unwrap()
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_with_input(BenchmarkId::new("sled store", &name), &response, |b, response| {
        b.iter_batched(
            || {
                let _dir = tempfile::tempdir().unwrap();
                let machine = OlmMachine::new(
                        alice_id(),
                        alice_device_id(),
                    );
                runtime
                    .block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response))
                    .unwrap();
                (machine, &runtime, &txn_id)
            },
            move |(machine, runtime, txn_id)| {
                runtime.block_on(machine.mark_request_as_sent(txn_id, response)).unwrap()
            },
            BatchSize::SmallInput,
        )
    });

    group.finish()
}

pub fn room_key_sharing(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");

    let keys_query_response = keys_query_response();
    let txn_id = TransactionId::new();
    let response = keys_claim_response();
    let room_id = room_id!("!test:localhost");

    let to_device_response = ToDeviceResponse::new();
    let users: Vec<Box<UserId>> = keys_query_response.device_keys.keys().cloned().collect();

    let count = response.one_time_keys.values().fold(0, |acc, d| acc + d.len());

    let machine = OlmMachine::new(alice_id(), alice_device_id());
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response)).unwrap();
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    let mut group = c.benchmark_group("Room key sharing");
    group.throughput(Throughput::Elements(count as u64));
    let name = format!("{} devices", count);

    group.bench_function(BenchmarkId::new("memory store", &name), |b| {
        b.to_async(&runtime).iter(|| async {
            let requests = machine
                .share_group_session(
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

            machine.invalidate_group_session(room_id).await.unwrap();
        })
    });

    let _dir = tempfile::tempdir().unwrap();
    let machine = OlmMachine::new(
            alice_id(),
            alice_device_id(),
        );
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response)).unwrap();
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    group.bench_function(BenchmarkId::new("sled store", &name), |b| {
        b.to_async(&runtime).iter(|| async {
            let requests = machine
                .share_group_session(
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

            machine.invalidate_group_session(room_id).await.unwrap();
        })
    });

    group.finish()
}

pub fn devices_missing_sessions_collecting(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");

    let machine = OlmMachine::new(alice_id(), alice_device_id());
    let response = huge_keys_query_response();
    let txn_id = TransactionId::new();
    let users: Vec<Box<UserId>> = response.device_keys.keys().cloned().collect();

    let count = response.device_keys.values().fold(0, |acc, d| acc + d.len());

    let mut group = c.benchmark_group("Devices missing sessions collecting");
    group.throughput(Throughput::Elements(count as u64));

    let name = format!("{} devices", count);

    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    group.bench_function(BenchmarkId::new("memory store", &name), |b| {
        b.to_async(&runtime).iter_with_large_drop(|| async {
            machine.get_missing_sessions(users.iter().map(Deref::deref)).await.unwrap()
        })
    });

    let _dir = tempfile::tempdir().unwrap();
    let machine = OlmMachine::new(
            alice_id(),
            alice_device_id(),
        );

    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    group.bench_function(BenchmarkId::new("sled store", &name), |b| {
        b.to_async(&runtime).iter(|| async {
            machine.get_missing_sessions(users.iter().map(Deref::deref)).await.unwrap()
        })
    });

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
    name = benches;
    config = criterion();
    targets = keys_query, keys_claiming, room_key_sharing, devices_missing_sessions_collecting,
}
criterion_main!(benches);
