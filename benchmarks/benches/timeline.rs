use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_test::{event_factory::EventFactory, JoinedRoomBuilder, StateTestEvent};
use matrix_sdk_ui::Timeline;
use ruma::{
    events::room::message::RoomMessageEventContentWithoutRelation, owned_room_id, owned_user_id,
    EventId,
};
use tokio::runtime::Builder;

/// Benchmark the time it takes to create a timeline (with read receipt
/// support), when there are many initial events at rest in the event cache.
///
/// `NUM_EVENTS` is the number of events that will be stored initially in the
/// event cache. It will be a mix of messages, reactions, edits and redactions,
/// so there are some aggregations to take into account by the timeline as well.
pub fn create_timeline_with_initial_events(c: &mut Criterion) {
    const NUM_EVENTS: usize = 10000;

    let runtime = Builder::new_multi_thread().enable_all().build().expect("Can't create runtime");
    let room_id = owned_room_id!("!room:example.com");

    let sender_id = owned_user_id!("@sender:example.com");
    let other_sender_id = owned_user_id!("@other_sender:example.com");
    let another_sender_id = owned_user_id!("@another_sender:example.com");

    let f = EventFactory::new().room(&room_id);

    let mut events = Vec::new();
    for i in 0..NUM_EVENTS {
        let sender = match i % 3 {
            0 => &sender_id,
            1 => &other_sender_id,
            2 => &another_sender_id,
            _ => unreachable!("math genius over here"),
        };

        let event_id = EventId::parse(format!("$event{i}")).unwrap();

        let j = i % 10;
        if j < 6 {
            // Messages.
            events.push(
                f.text_msg(format!("Message {i}"))
                    .sender(sender)
                    .event_id(&event_id)
                    .into_raw_sync(),
            );
        } else if j < 8 {
            // Reactions.
            let prev_event = EventId::parse(format!("$event{}", i - 2)).unwrap();
            events.push(
                f.reaction(&prev_event, "👍").sender(sender).event_id(&event_id).into_raw_sync(),
            );
        } else if j == 8 {
            // Edit.
            // Note: (i-3)%3 is the same as i%3 -> same sender!
            let prev_event = EventId::parse(format!("$event{}", i - 3)).unwrap();
            events.push(
                f.text_msg(format!("* Message {}v2", i - 3))
                    .edit(
                        &prev_event,
                        RoomMessageEventContentWithoutRelation::text_plain(format!(
                            "Message {}v2",
                            i - 3
                        )),
                    )
                    .sender(sender)
                    .event_id(&event_id)
                    .into_raw_sync(),
            );
        } else if j == 9 {
            // Redaction.
            // Note: (i-6)%3 is the same as i%6 -> same sender!
            let prev_event = EventId::parse(format!("$event{}", i - 6)).unwrap();
            events
                .push(f.redaction(&prev_event).sender(sender).event_id(&event_id).into_raw_sync());
        }
    }

    let builder = JoinedRoomBuilder::new(&room_id)
        .add_state_event(StateTestEvent::Encryption)
        .add_timeline_bulk(events);

    let room = runtime.block_on(async move {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        client.event_cache().subscribe().unwrap();

        let room = server.sync_room(&client, builder).await;
        drop(server);

        room
    });

    let mut group = c.benchmark_group("Test");
    group.throughput(Throughput::Elements(NUM_EVENTS as _));
    group.sample_size(10);

    group.bench_function(
        BenchmarkId::new("create_timeline_with_initial_events", format!("{NUM_EVENTS} events")),
        |b| {
            b.to_async(&runtime).iter(|| async {
                let timeline = Timeline::builder(&room)
                    .track_read_marker_and_receipts()
                    .build()
                    .await
                    .expect("Could not create timeline");

                let (items, _) = timeline.subscribe().await;
                assert_eq!(items.len(), 20);
            });
        },
    );

    group.finish();
}

fn criterion() -> Criterion {
    #[cfg(target_os = "linux")]
    {
        Criterion::default().with_profiler(pprof::criterion::PProfProfiler::new(
            100,
            pprof::criterion::Output::Flamegraph(None),
        ))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Criterion::default()
    }
}

criterion_group! {
    name = room;
    config = criterion();
    targets = create_timeline_with_initial_events
}
criterion_main!(room);
