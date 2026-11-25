//! Tests for [`matrix_sdk::Room::subscribe_to_members_changes`].

use std::pin::pin;

use matrix_sdk::{assert_next_with_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, User, async_test, event_factory::EventFactory};
#[cfg(feature = "unstable-msc4536")]
use ruma::{profile::ProfileFieldValue, room_id};
use stream_assert::assert_pending;

#[async_test]
async fn test_subscribe_to_members_changes_from_member_state() {
    // Given a room whose members changes are subscribed to.
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room = server.sync_default_joined_room(&client).await;

    let changes = room.subscribe_to_members_changes();
    let mut changes = pin!(changes);

    // Then nothing is reported until something changes.
    assert_pending!(changes);

    let f = EventFactory::new().default_room();

    // When Alice joins.
    server
        .sync_room(&client, JoinedRoomBuilder::default().add_state_event(f.member(User::Alice)))
        .await;

    // Then a single change is reported.
    assert_next_with_timeout!(changes);
    assert_pending!(changes);

    // When Bob joins, reported through `state_after`.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().use_state_after().add_state_event(f.member(User::Bob)),
        )
        .await;

    // Then a single change is reported.
    assert_next_with_timeout!(changes);
    assert_pending!(changes);

    // When a state event of another type is received.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_event(f.custom_state_event().sender(User::Alice)),
        )
        .await;

    // Then no change is reported.
    assert_pending!(changes);
}

#[async_test]
async fn test_subscribe_to_members_changes_from_power_levels() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Given a room whose members changes are subscribed to.
    let room = server.sync_default_joined_room(&client).await;

    let changes = room.subscribe_to_members_changes();
    let mut changes = pin!(changes);

    let f = EventFactory::new().default_room();

    // When Alice is promoted.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_state_event(
                f.power_levels().set_users([(User::Alice, 100)]).sender(User::Alice),
            ),
        )
        .await;

    // Then a single change is reported.
    assert_next_with_timeout!(changes);
    assert_pending!(changes);
}

#[cfg(feature = "unstable-msc4536")]
#[async_test]
async fn test_subscribe_to_members_changes_from_global_profiles() {
    let server = MatrixMockServer::new().await;
    server
        .mock_versions()
        .with_versions(vec!["v1.16"])
        .with_profiles_sliding_sync_extension()
        .with_query_profiles()
        .ok()
        .named("versions")
        .mount()
        .await;
    let client = server.client_builder().no_server_versions().build().await;

    let alice = User::Alice;
    let bob = User::Bob;

    // Given a room where Alice is joined, and another room where only Bob is.
    let f = EventFactory::new().default_room();
    let room = server
        .sync_room(&client, JoinedRoomBuilder::default().add_state_event(f.member(alice)))
        .await;

    let other_room_id = room_id!("!other:example.org");
    let f = EventFactory::new().room(other_room_id);
    let other_room = server
        .sync_room(&client, JoinedRoomBuilder::new(other_room_id).add_state_event(f.member(bob)))
        .await;

    let changes = room.subscribe_to_members_changes();
    let mut changes = pin!(changes);
    let other_changes = other_room.subscribe_to_members_changes();
    let mut other_changes = pin!(other_changes);

    server
        .mock_query_profiles()
        .expect_users(&[alice.into()])
        .ok_with_profiles(vec![(
            alice.into(),
            vec![ProfileFieldValue::DisplayName("Alice".to_owned())],
        )])
        .mock_once()
        .mount()
        .await;

    // When Alice's global profile is fetched.
    room.fetch_missing_global_profiles_in_background().await.unwrap();

    // Then the room she is in reports a change of its members.
    assert_next_with_timeout!(changes, 1000);
    assert_pending!(changes);

    // But not the room she isn't in.
    assert_pending!(other_changes);
}
