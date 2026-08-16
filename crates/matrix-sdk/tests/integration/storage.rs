use matrix_sdk::{
    media::{MediaFormat, MediaRequestParameters},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::RoomMemberships;
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{
    events::room::{MediaSource, member::MembershipState},
    owned_mxc_uri, room_id, user_id,
};
use tempfile::tempdir;

#[async_test]
#[cfg(feature = "sqlite")]
async fn test_storage_usage_and_room_clearing() {
    let tempdir = tempdir().unwrap();
    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| builder.sqlite_store(tempdir.path(), None))
        .build()
        .await;
    client.event_cache().subscribe().unwrap();

    let room_a = room_id!("!a:localhost");
    let room_b = room_id!("!b:localhost");
    let mxc = owned_mxc_uri!("mxc://localhost/img");

    // Room A: an image and two members; room B: a text.
    let fa = EventFactory::new().sender(*ALICE).room(room_a);
    let fb = EventFactory::new().sender(*ALICE).room(room_b);
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_a)
                .add_state_bulk([
                    fa.member(user_id!("@bob:localhost")).membership(MembershipState::Join).into(),
                    fa.member(user_id!("@carl:localhost")).membership(MembershipState::Join).into(),
                ])
                .add_timeline_event(fa.image("a.png".to_owned(), mxc.clone())),
        )
        .await;
    server
        .sync_room(&client, JoinedRoomBuilder::new(room_b).add_timeline_event(fb.text_msg("hi")))
        .await;
    // The event cache persists asynchronously.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // The image's content is cached.
    client
        .media_store()
        .lock()
        .await
        .unwrap()
        .add_media_content(
            &MediaRequestParameters {
                source: MediaSource::Plain(mxc.clone()),
                format: MediaFormat::File,
            },
            vec![0; 1000],
            matrix_sdk_base::media::store::IgnoreMediaRetentionPolicy::No,
        )
        .await
        .unwrap();

    let report = client.storage_usage().await.unwrap();
    assert!(report.events.per_room.contains_key(room_a), "{report:?}");
    assert!(report.events.per_room[room_a] > 0, "{report:?}");
    assert!(report.events.per_room.contains_key(room_b), "{report:?}");
    assert!(report.room_state.per_room[room_a] > report.room_state.per_room[room_b], "{report:?}");
    assert!(report.media.per_room[room_a] >= 1000, "{report:?}");
    assert!(!report.media.per_room.contains_key(room_b), "{report:?}");
    assert!(report.media.total_bytes >= 1000);

    // Clearing room A's caches empties its events and members, keeps the room
    // (and room B), and marks the members as missing, persistently.
    client.clear_room_caches(&[room_a.to_owned()]).await.unwrap();

    let before = report;
    let report = client.storage_usage().await.unwrap();
    assert!(!report.events.per_room.contains_key(room_a), "{report:?}");
    assert!(report.events.per_room[room_b] > 0, "{report:?}");
    // Room A's members are gone, its room info stays.
    assert!(report.room_state.per_room[room_a] < before.room_state.per_room[room_a], "{report:?}");
    assert!(report.room_state.per_room[room_a] > 0, "{report:?}");

    let room = client.get_room(room_a).unwrap();
    assert!(!room.are_members_synced());
    assert!(
        client
            .state_store()
            .get_user_ids(room_a, RoomMemberships::empty())
            .await
            .unwrap()
            .is_empty()
    );
    let infos = client.state_store().get_room_infos(&Default::default()).await.unwrap();
    let info = infos.iter().find(|info| info.room_id() == room_a).expect("room A is still known");
    assert!(!info.are_members_synced());

    // The media content wasn't touched, but it isn't attributable to the room
    // any more (its events are gone).
    assert!(report.media.total_bytes >= 1000);
    assert!(!report.media.per_room.contains_key(room_a));

    // Clearing all the media empties the store.
    client.clear_media_cache(None, None).await.unwrap();
    assert_eq!(client.storage_usage().await.unwrap().media.total_bytes, 0);
}
