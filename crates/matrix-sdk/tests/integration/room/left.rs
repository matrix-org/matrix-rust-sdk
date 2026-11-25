use matrix_sdk::{linked_chunk::LinkedChunkId, test_utils::mocks::MatrixMockServer};
use matrix_sdk_base::{RoomInfoNotableUpdateReasons, RoomState};
use matrix_sdk_test::{LeftRoomBuilder, TestRoom, User, async_test, event_factory::EventFactory};
use ruma::events::{GlobalAccountDataEventType, direct::DirectEventContent};
use strass::assert_let;
use tokio::task::yield_now;

#[async_test]
async fn test_forget_non_direct_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let room_id = TestRoom::Default;
    let f = EventFactory::new().sender(user_id);

    server.mock_room_forget().ok().named("forget").expect(1).mount().await;

    server
        .mock_set_global_account_data(user_id, &GlobalAccountDataEventType::Direct)
        .ok()
        .named("set_mdirect")
        .expect(0)
        .mount()
        .await;

    server
        .sync_room(&client, LeftRoomBuilder::default().add_timeline_event(f.text_msg("Goodbye!")))
        .await;

    // Let the event cache process updates.
    yield_now().await;

    {
        // There is some data in the cache store.
        let room_data = client
            .event_cache_store()
            .lock()
            .await
            .unwrap()
            .as_clean()
            .unwrap()
            .load_all_chunks(LinkedChunkId::Room(room_id.into()))
            .await
            .unwrap();
        assert!(!room_data.is_empty());
    }

    let room = client.get_room(room_id.into()).unwrap();
    assert_eq!(room.state(), RoomState::Left);

    room.forget().await.unwrap();

    assert!(client.get_room(room_id.into()).is_none());

    {
        // Data in the event cache store has been removed.
        let room_data = client
            .event_cache_store()
            .lock()
            .await
            .unwrap()
            .as_clean()
            .unwrap()
            .load_all_chunks(LinkedChunkId::Room(room_id.into()))
            .await
            .unwrap();
        assert!(room_data.is_empty());
    }
}

#[async_test]
async fn test_forget_banned_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let room_id = TestRoom::Default;
    let f = EventFactory::new().sender(user_id);

    server.mock_room_forget().ok().named("forget").expect(1).mount().await;

    server
        .mock_set_global_account_data(user_id, &GlobalAccountDataEventType::Direct)
        .ok()
        .named("set_mdirect")
        .expect(0)
        .mount()
        .await;

    server
        .sync_room(&client, LeftRoomBuilder::default().add_timeline_event(f.text_msg("Goodbye!")))
        .await;

    // Let the event cache process updates.
    yield_now().await;

    {
        // There is some data in the cache store.
        let room_data = client
            .event_cache_store()
            .lock()
            .await
            .unwrap()
            .as_clean()
            .unwrap()
            .load_all_chunks(LinkedChunkId::Room(room_id.into()))
            .await
            .unwrap();
        assert!(!room_data.is_empty());
    }

    // Make the room banned
    let room = client.get_room(room_id.into()).unwrap();
    room.update_room_info(|mut room_info| {
        room_info.mark_as_banned();
        (room_info, RoomInfoNotableUpdateReasons::MEMBERSHIP)
    })
    .await;
    assert_eq!(room.state(), RoomState::Banned);

    room.forget().await.unwrap();

    assert!(client.get_room(room_id.into()).is_none());

    {
        // Data in the event cache store has been removed.
        let room_data = client
            .event_cache_store()
            .lock()
            .await
            .unwrap()
            .as_clean()
            .unwrap()
            .load_all_chunks(LinkedChunkId::Room(room_id.into()))
            .await
            .unwrap();
        assert!(room_data.is_empty());
    }
}

#[async_test]
async fn test_forget_direct_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let invited_user_id = User::Bob;
    let room_id = TestRoom::Default;

    // Initialize the direct room.
    let f = EventFactory::new();

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_left_room(LeftRoomBuilder::default())
                .add_global_account_data(f.direct().add_user(invited_user_id, room_id));
        })
        .await;

    let room = client.get_room(room_id.into()).unwrap();
    assert_eq!(room.state(), RoomState::Left);
    assert!(room.is_direct().await.unwrap());
    assert!(room.direct_targets().contains(invited_user_id.as_str()));

    let direct_account_data = client
        .account()
        .account_data::<DirectEventContent>()
        .await
        .expect("getting m.direct account data failed")
        .expect("no m.direct account data")
        .deserialize()
        .expect("failed to deserialize m.direct account data");
    assert_let!(Some(invited_user_dms) = direct_account_data.get(invited_user_id.as_str()));
    assert_eq!(*invited_user_dms, [room_id]);

    server.mock_room_forget().ok().named("forget").expect(1).mount().await;

    server
        .mock_set_global_account_data(user_id, &GlobalAccountDataEventType::Direct)
        .ok()
        .named("set_mdirect")
        .expect(1)
        .mount()
        .await;

    room.forget().await.unwrap();

    assert!(client.get_room(room_id.into()).is_none());
}

#[async_test]
async fn test_rejoin_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = TestRoom::Default;

    server.mock_room_join(room_id).ok().expect(1).mount().await;

    let room = server.sync_room(&client, LeftRoomBuilder::default()).await;
    assert_eq!(room.state(), RoomState::Left);

    room.join().await.unwrap();
    assert!(!room.is_state_fully_synced())
}

#[async_test]
async fn test_knocking() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = TestRoom::Default;

    server.mock_room_knock(room_id).ok(room_id).expect(1).mount().await;

    let room = server.sync_room(&client, LeftRoomBuilder::default()).await;
    assert_eq!(room.state(), RoomState::Left);

    let room = client.knock(room_id.into(), None, Vec::new()).await.unwrap();
    assert_eq!(room.state(), RoomState::Knocked);
}
