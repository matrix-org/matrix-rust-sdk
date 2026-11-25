//! Tests for [`matrix_sdk::Room::subscribe_to_state_events`].
//!
//! They follow MatrixRTC call memberships, the use case this API exists for,
//! and a fully custom state event type.

use std::{collections::BTreeSet, pin::pin};

use matrix_sdk::{
    assert_next_with_timeout, deserialized_responses::RawAnySyncOrStrippedState,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{
    JoinedRoomBuilder, TestDevice, User, async_test, event_factory::EventFactory,
};
use ruma::events::StateEventType;
use strass::assert_let;
use stream_assert::assert_pending;

/// The `(type, sender)` pairs of a state snapshot of a joined room.
fn types_and_senders(state: &[RawAnySyncOrStrippedState]) -> BTreeSet<(String, String)> {
    state
        .iter()
        .map(|raw| {
            assert_let!(RawAnySyncOrStrippedState::Sync(raw) = raw);
            (
                raw.get_field::<String>("type").unwrap().unwrap(),
                raw.get_field::<String>("sender").unwrap().unwrap(),
            )
        })
        .collect()
}

#[async_test]
async fn test_subscribe_to_state_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_default_joined_room(&client).await;

    let stream = room.subscribe_to_state_events(StateEventType::CallMember);
    let mut stream = pin!(stream);

    // The current state comes first: nobody is in the call yet.
    let state = assert_next_with_timeout!(stream).unwrap();
    assert!(state.is_empty());

    let alice = User::Alice;
    let bob = User::Bob;
    let f = EventFactory::new().default_room();

    // Alice joins the call. The sync also carries a state event of another
    // type, which is not part of the snapshot.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_event(f.call_membership_state(alice, TestDevice::Default.into()))
                .add_state_event(f.custom_state_event().sender(alice)),
        )
        .await;

    let state = assert_next_with_timeout!(stream).unwrap();
    assert_eq!(
        types_and_senders(&state),
        BTreeSet::from([(StateEventType::CallMember.to_string(), alice.to_string())])
    );

    // Bob joins too, reported through `state_after`. The snapshot is the full
    // membership, not just the change.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .use_state_after()
                .add_state_event(f.call_membership_state(bob, TestDevice::Alt1.into())),
        )
        .await;

    let state = assert_next_with_timeout!(stream).unwrap();
    assert_eq!(
        types_and_senders(&state),
        BTreeSet::from([
            (StateEventType::CallMember.to_string(), alice.to_string()),
            (StateEventType::CallMember.to_string(), bob.to_string())
        ])
    );

    // A sync without any call membership change yields nothing.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_state_event(f.custom_state_event().sender(alice)),
        )
        .await;

    assert_pending!(stream);
}

#[async_test]
async fn test_subscribe_to_state_events_yields_one_snapshot_per_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_default_joined_room(&client).await;

    let stream = room.subscribe_to_state_events(StateEventType::CallMember);
    let mut stream = pin!(stream);

    // Skip the current state.
    assert_next_with_timeout!(stream).unwrap();

    let alice = User::Alice;
    let f = EventFactory::new().default_room();

    // Alice joins the call from two devices in the same sync: a single
    // snapshot, with both memberships.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_event(f.call_membership_state(alice, TestDevice::Default.into()))
                .add_state_event(f.call_membership_state(alice, TestDevice::Alt1.into())),
        )
        .await;

    let state = assert_next_with_timeout!(stream).unwrap();
    assert_eq!(state.len(), 2);

    assert_pending!(stream);
}

#[async_test]
async fn test_subscribe_to_state_events_ignores_the_timeline() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_default_joined_room(&client).await;

    let stream = room.subscribe_to_state_events(StateEventType::CallMember);
    let mut stream = pin!(stream);

    // Skip the current state.
    assert_next_with_timeout!(stream).unwrap();

    let f = EventFactory::new().default_room();

    // A call membership that only appears in the timeline updates the room
    // state, but is not what this subscription reports.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_event(
                f.call_membership_state(User::Alice, TestDevice::Default.into()),
            ),
        )
        .await;

    assert_eq!(room.get_state_events(StateEventType::CallMember).await.unwrap().len(), 1);
    assert_pending!(stream);
}

#[async_test]
async fn test_subscribe_to_custom_state_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_default_joined_room(&client).await;

    // A type the SDK knows nothing about works the same way.
    let event_type = StateEventType::from("rs.matrix-sdk.custom-state.test");
    let stream = room.subscribe_to_state_events(event_type.clone());
    let mut stream = pin!(stream);

    let state = assert_next_with_timeout!(stream).unwrap();
    assert!(state.is_empty());

    let alice = User::Alice;
    let f = EventFactory::new().default_room().sender(alice);

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_event(f.custom_state_event().state_key("first"))
                .add_state_event(f.custom_state_event().state_key("second")),
        )
        .await;

    let state = assert_next_with_timeout!(stream).unwrap();
    assert_eq!(state.len(), 2);
    assert_eq!(
        types_and_senders(&state),
        BTreeSet::from([(event_type.to_string(), alice.to_string())])
    );

    assert_pending!(stream);
}
