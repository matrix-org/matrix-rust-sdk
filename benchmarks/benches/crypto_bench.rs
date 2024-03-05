use std::{ops::Deref, sync::Arc};

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use matrix_sdk::{config::SyncSettings, test_utils::logged_in_client_with_server, Client};
use matrix_sdk_crypto::{EncryptionSettings, OlmMachine};
use matrix_sdk_sqlite::SqliteCryptoStore;
use matrix_sdk_test::{response_from_file, test_json, DEFAULT_TEST_ROOM_ID};
use ruma::{
    api::{
        client::{
            keys::{claim_keys, get_keys},
            to_device::send_event_to_device::v3::Response as ToDeviceResponse,
        },
        IncomingResponse,
    },
    device_id, room_id, user_id, DeviceId, OwnedUserId, TransactionId, UserId,
};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::runtime::Builder;
use wiremock::{
    matchers::{header, method, path, path_regex, query_param, query_param_is_missing},
    Mock, ResponseTemplate,
};

fn alice_id() -> &'static UserId {
    user_id!("@alice:example.org")
}

fn alice_device_id() -> &'static DeviceId {
    device_id!("JLAFKJWSCS")
}

pub async fn synced_client() -> (Client, wiremock::MockServer) {
    let (client, server) = logged_in_client_with_server().await;
    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new();

    let _response = client.sync_once(sync_settings).await.unwrap();

    (client, server)
}

/// Mount a Mock on the given server to handle the `GET /sync` endpoint with
/// an optional `since` param that returns a 200 status code with the given
/// response body.
async fn mock_sync(
    server: &wiremock::MockServer,
    response_body: impl Serialize,
    since: Option<String>,
) {
    let mut builder = wiremock::Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/sync"))
        .and(header("authorization", "Bearer 1234"));

    if let Some(since) = since {
        builder = builder.and(query_param("since", since));
    } else {
        builder = builder.and(query_param_is_missing("since"));
    }

    builder
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .mount(server)
        .await;
}

pub fn load_members_benchmark(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread().enable_all().build().expect("Can't create runtime");

    let members_in_room = 100000;

    let count = members_in_room;
    let mut group = c.benchmark_group("Keys querying");
    group.throughput(Throughput::Elements(count));

    let name = format!("{count} members");

    let (client, server) = runtime.block_on(synced_client());

    let mut members = Vec::new();
    let room_id = *DEFAULT_TEST_ROOM_ID;
    for i in 0..members_in_room {
        members.push(
            serde_json::from_str::<Value>(&format!(
                r#"{{
"content": {{
    "displayname": "User {i}",
    "membership": "join"
}},
"origin_server_ts": 1708337209586,
"room_id": "{room_id}",
"sender": "@sender:matrix.org",
"state_key": "@user_{i}:matrix.org",
"type": "m.room.member",
"event_id": "$eventid{i}"
}}"#
            ))
            .unwrap(),
        );
    }
    let response_body = json!({
        "chunk": members,
    });
    runtime.block_on(
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/members"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
            .mount(&server),
    );

    group.bench_function(BenchmarkId::new("memory store", &name), |b| {
        b.to_async(&runtime).iter(|| async {
            let room = client.get_room(room_id).unwrap();
            room.mark_members_missing();
            room.sync_members().await.unwrap();
        })
    });

    group.finish();
}

fn keys_query_response() -> get_keys::v3::Response {
    let data = include_bytes!("crypto_bench/keys_query.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    let data = response_from_file(&data);
    get_keys::v3::Response::try_from_http_response(data)
        .expect("Can't parse the `/keys/upload` response")
}

fn keys_claim_response() -> claim_keys::v3::Response {
    let data = include_bytes!("crypto_bench/keys_claim.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    let data = response_from_file(&data);
    claim_keys::v3::Response::try_from_http_response(data)
        .expect("Can't parse the `/keys/upload` response")
}

fn huge_keys_query_response() -> get_keys::v3::Response {
    let data = include_bytes!("crypto_bench/keys_query_2000_members.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    let data = response_from_file(&data);
    get_keys::v3::Response::try_from_http_response(data)
        .expect("Can't parse the `/keys/query` response")
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

    group.bench_with_input(BenchmarkId::new("memory store", &name), &response, |b, response| {
        b.to_async(&runtime)
            .iter(|| async { machine.mark_request_as_sent(&txn_id, response).await.unwrap() })
    });

    // Benchmark sqlite store.

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(runtime.block_on(SqliteCryptoStore::open(dir.path(), None)).unwrap());
    let machine =
        runtime.block_on(OlmMachine::with_store(alice_id(), alice_device_id(), store)).unwrap();

    group.bench_with_input(BenchmarkId::new("sqlite store", &name), &response, |b, response| {
        b.to_async(&runtime)
            .iter(|| async { machine.mark_request_as_sent(&txn_id, response).await.unwrap() })
    });

    {
        let _guard = runtime.enter();
        drop(machine);
    }

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

    let name = format!("{count} one-time keys");

    group.bench_with_input(BenchmarkId::new("memory store", &name), &response, |b, response| {
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
            BatchSize::SmallInput,
        )
    });

    group.bench_with_input(BenchmarkId::new("sqlite store", &name), &response, |b, response| {
        b.iter_batched(
            || {
                let dir = tempfile::tempdir().unwrap();
                let store =
                    Arc::new(runtime.block_on(SqliteCryptoStore::open(dir.path(), None)).unwrap());

                let machine = runtime
                    .block_on(OlmMachine::with_store(alice_id(), alice_device_id(), store))
                    .unwrap();
                runtime
                    .block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response))
                    .unwrap();
                (machine, &runtime, &txn_id)
            },
            move |(machine, runtime, txn_id)| {
                runtime.block_on(async {
                    machine.mark_request_as_sent(txn_id, response).await.unwrap();
                    drop(machine)
                })
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
    let users: Vec<OwnedUserId> = keys_query_response.device_keys.keys().cloned().collect();

    let count = response.one_time_keys.values().fold(0, |acc, d| acc + d.len());

    let machine = runtime.block_on(OlmMachine::new(alice_id(), alice_device_id()));
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response)).unwrap();
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    let mut group = c.benchmark_group("Room key sharing");
    group.throughput(Throughput::Elements(count as u64));
    let name = format!("{count} devices");

    // Benchmark memory store.

    group.bench_function(BenchmarkId::new("memory store", &name), |b| {
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

    let machine =
        runtime.block_on(OlmMachine::with_store(alice_id(), alice_device_id(), store)).unwrap();
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &keys_query_response)).unwrap();
    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    group.bench_function(BenchmarkId::new("sqlite store", &name), |b| {
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

    group.bench_function(BenchmarkId::new("memory store", &name), |b| {
        b.to_async(&runtime).iter_with_large_drop(|| async {
            machine.get_missing_sessions(users.iter().map(Deref::deref)).await.unwrap()
        })
    });

    // Benchmark sqlite store.

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(runtime.block_on(SqliteCryptoStore::open(dir.path(), None)).unwrap());

    let machine =
        runtime.block_on(OlmMachine::with_store(alice_id(), alice_device_id(), store)).unwrap();

    runtime.block_on(machine.mark_request_as_sent(&txn_id, &response)).unwrap();

    group.bench_function(BenchmarkId::new("sqlite store", &name), |b| {
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
    targets = load_members_benchmark, keys_query, keys_claiming, room_key_sharing, devices_missing_sessions_collecting,
}
criterion_main!(benches);
