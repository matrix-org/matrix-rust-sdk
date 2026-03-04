use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_test::{JoinedRoomBuilder, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{TimelineBuilder, TimelineReadReceiptTracking};
use ruma::{
    OwnedEventId, events::room::message::RoomMessageEventContentWithoutRelation, owned_room_id,
    owned_user_id,
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
    let room_id = owned_room_id!("!fortesfortunajuvat:example.com");

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

        let j = i % 10;
        if j < 6 {
            // Messages.
            events.push(f.text_msg(format!("Message {i}")).sender(sender).into_raw_sync());
        } else if j < 8 {
            // Reactions.
            let prev_event_id = events[i - 2]
                .get_field::<OwnedEventId>("event_id")
                .expect("invalid event ID")
                .expect("missing event ID");
            events.push(f.reaction(&prev_event_id, "👍").sender(sender).into_raw_sync());
        } else if j == 8 {
            // Edit.
            // Note: (i-3)%3 is the same as i%3 -> same sender!
            let prev_event_id = events[i - 3]
                .get_field::<OwnedEventId>("event_id")
                .expect("invalid event ID")
                .expect("missing event ID");
            events.push(
                f.text_msg(format!("* Message {}v2", i - 3))
                    .edit(
                        &prev_event_id,
                        RoomMessageEventContentWithoutRelation::text_plain(format!(
                            "Message {}v2",
                            i - 3
                        )),
                    )
                    .sender(sender)
                    .into_raw_sync(),
            );
        } else if j == 9 {
            // Redaction.
            // Note: (i-6)%3 is the same as i%6 -> same sender!
            let prev_event_id = events[i - 6]
                .get_field::<OwnedEventId>("event_id")
                .expect("invalid event ID")
                .expect("missing event ID");
            events.push(f.redaction(&prev_event_id).sender(sender).into_raw_sync());
        }
    }

    let builder = JoinedRoomBuilder::new(&room_id)
        .add_state_event(f.room_encryption().sender(&sender_id))
        .add_timeline_bulk(events);

    let room = runtime.block_on(async move {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        client.event_cache().subscribe().unwrap();

        let room = server.sync_room(&client, builder).await;
        drop(server);

        room
    });

    let mut group = c.benchmark_group("Create a timeline");
    group.throughput(Throughput::Elements(NUM_EVENTS as _));
    group.sample_size(10);

    group.bench_function(
        BenchmarkId::new("Create a timeline with initial events", format!("{NUM_EVENTS} events")),
        |b| {
            b.to_async(&runtime).iter(|| async {
                let timeline = TimelineBuilder::new(&room)
                    .track_read_marker_and_receipts(TimelineReadReceiptTracking::AllEvents)
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

criterion_group! {
    name = room;
    config = Criterion::default();
    targets = create_timeline_with_initial_events
}
criterion_main!(room);
