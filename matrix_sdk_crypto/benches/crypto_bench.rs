#[cfg(target_os = "linux")]
mod perf;

use std::convert::TryFrom;

use criterion::{async_executor::FuturesExecutor, *};

use futures::executor::block_on;
use matrix_sdk_common::{
    api::r0::keys::{claim_keys, get_keys},
    identifiers::{user_id, DeviceIdBox, UserId},
    uuid::Uuid,
};
use matrix_sdk_crypto::OlmMachine;
use matrix_sdk_test::response_from_file;
use serde_json::Value;

fn alice_id() -> UserId {
    user_id!("@alice:example.org")
}

fn alice_device_id() -> DeviceIdBox {
    "JLAFKJWSCS".into()
}

fn keys_query_response() -> get_keys::Response {
    let data = include_bytes!("./keys_query.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    let data = response_from_file(&data);
    get_keys::Response::try_from(data).expect("Can't parse the keys upload response")
}

fn keys_claim_response() -> claim_keys::Response {
    let data = include_bytes!("./keys_claim.json");
    let data: Value = serde_json::from_slice(data).unwrap();
    let data = response_from_file(&data);
    claim_keys::Response::try_from(data).expect("Can't parse the keys upload response")
}

pub fn keys_query(c: &mut Criterion) {
    let machine = OlmMachine::new(&alice_id(), &alice_device_id());
    let response = keys_query_response();
    let uuid = Uuid::new_v4();

    let count = response
        .device_keys
        .values()
        .fold(0, |acc, d| acc + d.len())
        + response.master_keys.len()
        + response.self_signing_keys.len()
        + response.user_signing_keys.len();

    let mut group = c.benchmark_group("Keys querying");
    group.throughput(Throughput::Elements(count as u64));

    group.bench_with_input(
        BenchmarkId::new(
            "Keys querying",
            "150 device keys parsing and signature checking",
        ),
        &response,
        |b, response| {
            b.to_async(FuturesExecutor)
                .iter(|| async { machine.mark_request_as_sent(&uuid, response).await.unwrap() })
        },
    );
    group.finish()
}

pub fn keys_claiming(c: &mut Criterion) {
    let keys_query_response = keys_query_response();
    let uuid = Uuid::new_v4();

    let response = keys_claim_response();

    let count = response
        .one_time_keys
        .values()
        .fold(0, |acc, d| acc + d.len());

    let mut group = c.benchmark_group("Keys claiming throughput");
    group.throughput(Throughput::Elements(count as u64));

    let name = format!("{} one-time keys claiming and session creation", count);

    group.bench_with_input(
        BenchmarkId::new("One-time keys claiming", &name),
        &response,
        |b, response| {
            b.iter_batched(
                || {
                    let machine = OlmMachine::new(&alice_id(), &alice_device_id());
                    block_on(machine.mark_request_as_sent(&uuid, &keys_query_response)).unwrap();
                    machine
                },
                move |machine| block_on(machine.mark_request_as_sent(&uuid, response)).unwrap(),
                BatchSize::SmallInput,
            )
        },
    );
    group.finish()
}

fn criterion() -> Criterion {
    #[cfg(target_os = "linux")]
    let criterion = Criterion::default().with_profiler(perf::FlamegraphProfiler::new(100));
    #[cfg(not(target_os = "linux"))]
    let criterion = Criterion::default();

    criterion
}

criterion_group! {
    name = benches;
    config = criterion();
    targets = keys_query, keys_claiming
}
criterion_main!(benches);
