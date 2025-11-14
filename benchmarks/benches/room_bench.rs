use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use matrix_sdk::{store::RoomLoadSettings, test_utils::mocks::MatrixMockServer};
use matrix_sdk_base::{
    BaseClient, RoomInfo, RoomState, SessionMeta, StateChanges, StateStore, ThreadingSupport,
    store::StoreConfig,
};
use matrix_sdk_sqlite::SqliteStateStore;
use matrix_sdk_test::{JoinedRoomBuilder, StateTestEvent, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{TimelineBuilder, TimelineFocus};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
    api::client::membership::get_member_events,
    device_id,
    events::room::member::{MembershipState, RoomMemberEvent},
    mxc_uri, owned_room_id, owned_user_id,
    serde::Raw,
    user_id,
};
use serde_json::json;
use tokio::runtime::Builder;
use wiremock::{Request, ResponseTemplate};

pub fn receive_all_members_benchmark(c: &mut Criterion) {
    const MEMBERS_IN_ROOM: usize = 100000;

    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");
    let room_id = owned_room_id!("!room:example.com");

    let f = EventFactory::new().room(&room_id);
    let mut member_events: Vec<Raw<RoomMemberEvent>> = Vec::with_capacity(MEMBERS_IN_ROOM);
    for i in 0..MEMBERS_IN_ROOM {
        let user_id = OwnedUserId::try_from(format!("@user_{i}:matrix.org")).unwrap();
        let event = f
            .member(&user_id)
            .membership(MembershipState::Join)
            .avatar_url(mxc_uri!("mxc://example.org/SEsfnsuifSDFSSEF"))
            .display_name("Alice Margatroid")
            .reason("Looking for support")
            .into_raw();
        member_events.push(event);
    }

    // Create a fake list of changes, and a session to recover from.
    let mut changes = StateChanges::default();
    changes.add_room(RoomInfo::new(&room_id, RoomState::Joined));
    for member_event in member_events.iter() {
        let event = member_event.clone().cast();
        changes.add_state_event(&room_id, event.deserialize().unwrap(), event);
    }

    // Sqlite
    let sqlite_dir = tempfile::tempdir().unwrap();
    let sqlite_store = runtime.block_on(SqliteStateStore::open(sqlite_dir.path(), None)).unwrap();
    runtime
        .block_on(sqlite_store.save_changes(&changes))
        .expect("initial filling of sqlite failed");

    let base_client = BaseClient::new(
        StoreConfig::new("cross-process-store-locks-holder-name".to_owned())
            .state_store(sqlite_store),
        ThreadingSupport::Disabled,
    );

    runtime
        .block_on(base_client.activate(
            SessionMeta {
                user_id: user_id!("@somebody:example.com").to_owned(),
                device_id: device_id!("DEVICE_ID").to_owned(),
            },
            RoomLoadSettings::default(),
            None,
        ))
        .expect("Could not set session meta");

    base_client.get_or_create_room(&room_id, RoomState::Joined);

    let request = get_member_events::v3::Request::new(room_id.clone());
    let response = get_member_events::v3::Response::new(member_events);

    let count = MEMBERS_IN_ROOM;
    let name = format!("{count} members");
    let mut group = c.benchmark_group("Test");
    group.throughput(Throughput::Elements(count as u64));
    group.sample_size(50);

    group.bench_function(BenchmarkId::new("Handle /members request [SQLite]", name), |b| {
        b.to_async(&runtime).iter(|| async {
            base_client.receive_all_members(&room_id, &request, &response).await.unwrap();
        });
    });

    {
        let _guard = runtime.enter();
        drop(base_client);
    }

    group.finish();
}

pub fn load_pinned_events_benchmark(c: &mut Criterion) {
    const PINNED_EVENTS_COUNT: usize = 100;

    let runtime = Builder::new_multi_thread().enable_all().build().expect("Can't create runtime");
    let room_id = owned_room_id!("!room:example.com");
    let sender_id = owned_user_id!("@sender:example.com");

    let f = EventFactory::new().room(&room_id).sender(&sender_id);

    let mut joined_room_builder =
        JoinedRoomBuilder::new(&room_id).add_state_event(StateTestEvent::Encryption);

    let pinned_event_ids: Vec<OwnedEventId> = (0..PINNED_EVENTS_COUNT)
        .map(|i| EventId::parse(format!("${i}")).expect("Invalid event id"))
        .collect();
    joined_room_builder = joined_room_builder.add_state_event(StateTestEvent::Custom(json!(
        {
            "content": {
                "pinned": pinned_event_ids
            },
            "event_id": "$15139375513VdeRF:localhost",
            "origin_server_ts": 151393755,
            "sender": "@example:localhost",
            "state_key": "",
            "type": "m.room.pinned_events",
            "unsigned": {
                "age": 703422
            }
        }
    )));

    let (server, client, room) = runtime.block_on(async move {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room = server.sync_room(&client, joined_room_builder).await;

        server
            .mock_room_event()
            .respond_with(move |r: &Request| {
                let segments: Vec<&str> = r.url.path_segments().expect("Invalid path").collect();
                let event_id_str = segments[6];
                let event_id = EventId::parse(event_id_str).expect("Invalid event id in response");
                let event = f
                    .text_msg(format!("Message {event_id_str}"))
                    .event_id(&event_id)
                    .server_ts(MilliSecondsSinceUnixEpoch::now())
                    .into_raw_sync();
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(50))
                    .set_body_json(event.json())
            })
            .mount()
            .await;

        client.event_cache().subscribe().unwrap();

        (server, client, room)
    });

    let pinned_event_ids = room.pinned_event_ids().unwrap_or_default();
    assert!(!pinned_event_ids.is_empty());
    assert_eq!(pinned_event_ids.len(), PINNED_EVENTS_COUNT);

    let count = PINNED_EVENTS_COUNT;
    let name = format!("{count} pinned events");
    let mut group = c.benchmark_group("Load pinned events");
    group.throughput(Throughput::Elements(count as u64));
    group.sample_size(10);

    group.bench_function(BenchmarkId::new("Load pinned events [memory]", name), |b| {
        b.to_async(&runtime).iter(|| async {
            let pinned_event_ids = room.pinned_event_ids().unwrap_or_default();
            assert!(!pinned_event_ids.is_empty());
            assert_eq!(pinned_event_ids.len(), PINNED_EVENTS_COUNT);

            // Reset cache so it always loads the events from the mocked endpoint
            client
                .event_cache_store()
                .lock()
                .await
                .unwrap()
                .as_clean()
                .unwrap()
                .clear_all_linked_chunks()
                .await
                .unwrap();

            let timeline = TimelineBuilder::new(&room)
                .with_focus(TimelineFocus::PinnedEvents {
                    max_events_to_load: 100,
                    max_concurrent_requests: 10,
                })
                .build()
                .await
                .expect("Could not create timeline");

            let (items, _) = timeline.subscribe().await;
            assert_eq!(items.len(), PINNED_EVENTS_COUNT + 1);
            timeline.clear().await;
        });
    });

    {
        let _guard = runtime.enter();
        drop(server);
    }

    group.finish();
}

criterion_group! {
    name = room;
    config = Criterion::default();
    targets = receive_all_members_benchmark, load_pinned_events_benchmark,
}
criterion_main!(room);
