use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use matrix_sdk::utils::IntoRawStateEventContent;
use matrix_sdk_base::{
    store::StoreConfig, BaseClient, RoomInfo, RoomState, SessionMeta, StateChanges, StateStore,
};
use matrix_sdk_sqlite::SqliteStateStore;
use matrix_sdk_test::EventBuilder;
use ruma::{
    api::client::membership::get_member_events,
    device_id,
    events::room::member::{RoomMemberEvent, RoomMemberEventContent},
    owned_room_id,
    serde::Raw,
    user_id, OwnedUserId,
};
use serde_json::json;
use tokio::runtime::Builder;

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
    targets = receive_all_members_benchmark,
}
criterion_main!(room);
