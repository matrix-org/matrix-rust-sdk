use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use matrix_sdk::{
    config::SyncSettings,
    test_utils::{events::EventFactory, logged_in_client_with_server},
    utils::IntoRawStateEventContent,
};
use matrix_sdk_base::{
    store::StoreConfig, BaseClient, RoomInfo, RoomState, SessionMeta, StateChanges, StateStore,
};
use matrix_sdk_sqlite::SqliteStateStore;
use matrix_sdk_test::{EventBuilder, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder};
use matrix_sdk_ui::{timeline::TimelineFocus, Timeline};
use ruma::{
    api::client::membership::get_member_events,
    device_id,
    events::room::member::{RoomMemberEvent, RoomMemberEventContent},
    owned_room_id, owned_user_id,
    serde::Raw,
    user_id, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
};
use serde::Serialize;
use serde_json::json;
use tokio::runtime::Builder;
use wiremock::{
    matchers::{header, method, path, path_regex, query_param, query_param_is_missing},
    Mock, MockServer, Request, ResponseTemplate,
};

pub fn receive_all_members_benchmark(c: &mut Criterion) {
    const MEMBERS_IN_ROOM: usize = 100000;

    let runtime = Builder::new_multi_thread().build().expect("Can't create runtime");
    let room_id = owned_room_id!("!room:example.com");

    let ev_builder = EventBuilder::new();
    let mut member_events: Vec<Raw<RoomMemberEvent>> = Vec::with_capacity(MEMBERS_IN_ROOM);
    let member_content_json = json!({
        "avatar_url": "mxc://example.org/SEsfnsuifSDFSSEF",
        "displayname": "Alice Margatroid",
        "membership": "join",
        "reason": "Looking for support",
    });
    let member_content: Raw<RoomMemberEventContent> =
        member_content_json.into_raw_state_event_content().cast();
    for i in 0..MEMBERS_IN_ROOM {
        let user_id = OwnedUserId::try_from(format!("@user_{}:matrix.org", i)).unwrap();
        let state_key = user_id.to_string();
        let event: Raw<RoomMemberEvent> = ev_builder
            .make_state_event(
                &user_id,
                &room_id,
                &state_key,
                member_content.deserialize().unwrap(),
                None,
            )
            .cast();
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

    let base_client = BaseClient::with_store_config(StoreConfig::new().state_store(sqlite_store));

    runtime
        .block_on(base_client.set_session_meta(
            SessionMeta {
                user_id: user_id!("@somebody:example.com").to_owned(),
                device_id: device_id!("DEVICE_ID").to_owned(),
            },
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

    group.bench_function(BenchmarkId::new("receive_members", name), |b| {
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
    let (client, server) = runtime.block_on(logged_in_client_with_server());

    let mut sync_response_builder = SyncResponseBuilder::new();
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
    let response_json =
        sync_response_builder.add_joined_room(joined_room_builder).build_json_sync_response();
    runtime.block_on(mock_sync(&server, response_json, None));

    let sync_settings = SyncSettings::default();
    runtime.block_on(client.sync_once(sync_settings)).expect("Could not sync");
    runtime.block_on(server.reset());

    runtime.block_on(
        Mock::given(method("GET"))
            .and(path_regex(r"/_matrix/client/r0/rooms/.*/event/.*"))
            .respond_with(move |r: &Request| {
                let segments: Vec<&str> = r.url.path_segments().expect("Invalid path").collect();
                let event_id_str = segments[6];
                // let f = EventFactory::new().room(&room_id)
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
            .mount(&server),
    );
    // runtime.block_on(server.reset());

    client.event_cache().subscribe().unwrap();

    let room = client.get_room(&room_id).expect("Room not found");
    assert!(!room.pinned_event_ids().is_empty());
    assert_eq!(room.pinned_event_ids().len(), PINNED_EVENTS_COUNT);

    let count = PINNED_EVENTS_COUNT;
    let name = format!("{count} pinned events");
    let mut group = c.benchmark_group("Test");
    group.throughput(Throughput::Elements(count as u64));
    group.sample_size(10);

    group.bench_function(BenchmarkId::new("load_pinned_events", name), |b| {
        b.to_async(&runtime).iter(|| async {
            assert!(!room.pinned_event_ids().is_empty());
            assert_eq!(room.pinned_event_ids().len(), PINNED_EVENTS_COUNT);

            // Reset cache so it always loads the events from the mocked endpoint
            client.event_cache().empty_immutable_cache().await;

            let timeline = Timeline::builder(&room)
                .with_focus(TimelineFocus::PinnedEvents { max_events_to_load: 100 })
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
        runtime.block_on(server.reset());
        drop(client);
        drop(server);
    }

    group.finish();
}

async fn mock_sync(server: &MockServer, response_body: impl Serialize, since: Option<String>) {
    let mut mock_builder = Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/sync"))
        .and(header("authorization", "Bearer 1234"));

    if let Some(since) = since {
        mock_builder = mock_builder.and(query_param("since", since));
    } else {
        mock_builder = mock_builder.and(query_param_is_missing("since"));
    }

    mock_builder
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .mount(server)
        .await;
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
    name = room;
    config = criterion();
    targets = receive_all_members_benchmark, load_pinned_events_benchmark,
}
criterion_main!(room);
