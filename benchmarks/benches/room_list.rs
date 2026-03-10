use assert_matches::assert_matches;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use futures_util::pin_mut;
use matrix_sdk::{stream::StreamExt, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, base64_sha256_hash, event_factory::EventFactory};
use matrix_sdk_ui::{
    RoomListService, eyeball_im::VectorDiff, room_list_service::filters::new_filter_non_left,
};
use rand::{distributions::Uniform, prelude::Distribution};
use ruma::{OwnedRoomId, RoomId, owned_user_id};
use tokio::runtime::Builder;

/// Benchmark the time it takes to create a room list.
pub fn create(c: &mut Criterion) {
    const NUMBER_OF_ROOMS: usize = 1000;
    const NUMBER_OF_EVENTS_PER_ROOM: usize = 1000;

    let runtime = Builder::new_multi_thread().enable_all().build().expect("Can't create runtime");

    let (server, client) = runtime.block_on(async {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        client.event_cache().subscribe().unwrap();

        (server, client)
    });

    let sender_id = owned_user_id!("@mnt_io:matrix.org");
    let mut rand = rand::thread_rng();
    let server_ts_range = Uniform::from(100..1000);

    for room_nth in 0..NUMBER_OF_ROOMS {
        // Synapse's room IDs for rooms v1 to v11 have an 18 characters localpart.
        let raw_room_id = format!("!arsgratiaartis{room_nth:04}:example.com");

        let room_id = if room_nth % 10 == 9 {
            // Make 1 in 10 rooms use a room v12 ID, which is a base64 hash similar to an
            // event ID.
            RoomId::new_v2(&base64_sha256_hash(raw_room_id.as_bytes())).unwrap()
        } else {
            OwnedRoomId::try_from(raw_room_id).unwrap()
        };

        let first_server_ts = server_ts_range.sample(&mut rand);
        let event_factory = EventFactory::new().room(&room_id).server_ts(first_server_ts);

        let events = (0..NUMBER_OF_EVENTS_PER_ROOM)
            .map(|event_nth| {
                event_factory
                    .text_msg(format!("a {room_nth}_{event_nth}"))
                    .sender(&sender_id)
                    .into_raw_sync()
            })
            .collect::<Vec<_>>();

        let _room = runtime.block_on(async {
            server
                .sync_room(&client, JoinedRoomBuilder::new(&room_id).add_timeline_bulk(events))
                .await
        });
    }

    let mut group = c.benchmark_group("RoomList");
    group.throughput(Throughput::Elements(NUMBER_OF_ROOMS.try_into().unwrap()));

    group.bench_function(
        BenchmarkId::new(
            "Create",
            format!("{NUMBER_OF_ROOMS} rooms × {NUMBER_OF_EVENTS_PER_ROOM} events"),
        ),
        |bencher| {
            bencher.to_async(&runtime).iter(|| async {
                let room_list_service = RoomListService::new(client.clone())
                    .await
                    .expect("build the room list service");
                let room_list = room_list_service.all_rooms().await.expect("fetch `all_rooms`");
                let (entries_stream, entries_controller) =
                    room_list.entries_with_dynamic_adapters(20);

                // Setting the filter will trigger the entries stream computation.
                entries_controller.set_filter(Box::new(new_filter_non_left()));

                pin_mut!(entries_stream);
                let update = entries_stream.next().await.expect("receiving the reset update");
                assert_eq!(update.len(), 1);
                assert_matches!(&update[0], VectorDiff::Reset { values } => {
                    assert_eq!(values.len(), 20);
                });
            });
        },
    );

    group.finish();
}

criterion_group! {
    name = room_list;
    config = Criterion::default();
    targets = create
}
criterion_main!(room_list);
