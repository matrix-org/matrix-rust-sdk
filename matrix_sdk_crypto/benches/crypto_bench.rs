use std::convert::TryFrom;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use futures::executor::block_on;
use matrix_sdk_common::{
    api::r0::keys::get_keys,
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

pub fn receive_keys_query(c: &mut Criterion) {
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

    let mut group = c.benchmark_group("key query throughput");
    group.throughput(Throughput::Elements(count as u64));

    group.bench_with_input(
        BenchmarkId::new("key_query", "150 devices key query response parsing"),
        &response,
        |b, response| b.iter(|| block_on(machine.mark_request_as_sent(&uuid, response)).unwrap()),
    );
    group.finish()
}

criterion_group!(benches, receive_keys_query);
criterion_main!(benches);
