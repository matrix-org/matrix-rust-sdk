use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::test_utils::logged_in_client_with_server;
use matrix_sdk_ui::{room_list_service::filters::new_filter_non_left, RoomListService};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::runtime::Builder;
use wiremock::{http::Method, Match, Mock, Request, ResponseTemplate};

struct SlidingSyncMatcher;

impl Match for SlidingSyncMatcher {
    fn matches(&self, request: &Request) -> bool {
        request.url.path() == "/_matrix/client/unstable/org.matrix.msc3575/sync"
            && request.method == Method::POST
    }
}

#[derive(Deserialize)]
pub(crate) struct PartialSlidingSyncRequest {
    pub txn_id: Option<String>,
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

pub fn room_list(c: &mut Criterion) {
    let runtime = Builder::new_multi_thread()
        .enable_time()
        .enable_io()
        .build()
        .expect("Can't create runtime");

    let mut benchmark = c.benchmark_group("room_list");

    for number_of_rooms in [10, 100, 1000, 10_000] {
        benchmark.throughput(Throughput::Elements(number_of_rooms));
        benchmark.bench_with_input(
            BenchmarkId::new("sync", number_of_rooms),
            &number_of_rooms,
            |benchmark, maximum_number_of_rooms| {
                benchmark.iter_batched(
                    || {
                        runtime.block_on(async {
                            // Create a fresh new `Client` and a mocked server.
                            let (client, server) = logged_in_client_with_server().await;

                            // Create a new `RoomListService`.
                            let room_list_service = RoomListService::new(client.clone())
                                .await
                                .expect("Failed to create the `RoomListService`");

                            // Compute the response.
                            let rooms = {
                                let mut room_nth = 0u32;

                                (0..*maximum_number_of_rooms)
                                    .map(|_| {
                                        let room_id = format!("!r{room_nth}:matrix.org");
                                        let room = json!({
                                            "bump_stamp": room_nth % 10,
                                            "required_state": [{
                                                "content": {
                                                    "name": format!("Room #{room_nth}"),
                                                },
                                                "event_id": format!("$s{room_nth}"),
                                                "origin_server_ts": 1,
                                                "sender": "@example:matrix.org",
                                                "state_key": "",
                                                "type": "m.room.name",
                                            }],
                                            "timeline": [{
                                                "event_id": format!("$t{room_nth}"),
                                                "sender": "@example:matrix.org",
                                                "type": "m.room.message",
                                                "content": {
                                                    "body": "foo",
                                                    "msgtype": "m.text",
                                                },
                                                "origin_server_ts": 1,
                                            }],
                                        });

                                        room_nth += 1;

                                        (room_id, room)
                                    })
                                    .collect::<Map<String, Value>>()
                            };

                            // Mock the response from the server.
                            let _mock_guard = Mock::given(SlidingSyncMatcher)
                                .respond_with(move |request: &Request| {
                                    let partial_request: PartialSlidingSyncRequest =
                                        request.body_json().unwrap();

                                    ResponseTemplate::new(200).set_body_json(json!({
                                        "txn_id": partial_request.txn_id,
                                        "pos": "raclette",
                                        "rooms": rooms,
                                    }))
                                })
                                .mount_as_scoped(&server)
                                .await;

                            (&runtime, client, server, room_list_service)
                        })
                    },
                    |(runtime, _client, _server, room_list_service)| {
                        runtime.block_on(async {
                            // Get the `RoomListService` sync stream.
                            let room_list_stream = room_list_service.sync();
                            pin_mut!(room_list_stream);

                            // Get the `RoomList` itself with a default filter.
                            let room_list = room_list_service
                                .all_rooms()
                                .await
                                .expect("Failed to fetch the `all_rooms` room list");

                            let (room_list_entries, room_list_entries_controller) =
                                room_list.entries_with_dynamic_adapters(100);
                            pin_mut!(room_list_entries);

                            room_list_entries_controller
                                .set_filter(Box::new(new_filter_non_left()));
                            room_list_entries.next().await;

                            // Sync the room list service.
                            assert!(
                                room_list_stream.next().await.is_some(),
                                "`room_list_stream` has stopped"
                            );

                            // Sync the room list entries.
                            assert!(
                                room_list_entries.next().await.is_some(),
                                "`room_list_entries` has stopped"
                            );
                        })
                    },
                    BatchSize::SmallInput,
                )
            },
        );
    }

    benchmark.finish();
}

criterion_group! {
    name = benches;
    config = criterion();
    targets = room_list
}
criterion_main!(benches);
