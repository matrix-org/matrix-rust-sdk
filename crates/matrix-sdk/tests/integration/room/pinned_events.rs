use std::ops::Not as _;

use matrix_sdk::{Room, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, StateTestEvent, async_test, event_factory::EventFactory};
use ruma::{EventId, event_id, owned_event_id, room_id, user_id};
use serde_json::json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path_regex},
};

struct PinningTestSetup<'a> {
    event_id: &'a EventId,
    room_id: &'a ruma::RoomId,
    room: Room,
    client: matrix_sdk::Client,
    server: MatrixMockServer,
}

impl PinningTestSetup<'_> {
    async fn new() -> Self {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a98sd12bjh:example.org");
        let room = server.sync_joined_room(&client, room_id).await;

        server.mock_room_state_encryption().plain().mount().await;

        // This is necessary to get an empty list of pinned events when there are no
        // pinned events state event in the required state.
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.pinned_events/.*"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({})))
            .mount(server.server())
            .await;

        let event_id = event_id!("$a");
        Self { event_id, room_id, client, server, room }
    }

    async fn mock_sync(&mut self, include_pinned_state_event: bool) {
        let f = EventFactory::new().sender(user_id!("@a:b.c"));

        let mut joined_room_builder = JoinedRoomBuilder::new(self.room_id)
            .add_timeline_event(f.text_msg("A").event_id(self.event_id).into_raw_sync());

        if include_pinned_state_event {
            joined_room_builder =
                joined_room_builder.add_state_event(StateTestEvent::RoomPinnedEvents);
        }

        self.server.sync_room(&self.client, joined_room_builder).await;
    }
}

#[async_test]
async fn test_pin_event_is_sent_successfully() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(false).await;

    // Pinning a remote event succeeds.
    setup.server.mock_set_room_pinned_events().ok(owned_event_id!("$42")).mock_once().mount().await;

    assert!(setup.room.pin_event(setup.event_id).await.unwrap());
}

#[async_test]
async fn test_pin_event_is_returning_false_because_is_already_pinned() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(true).await;

    assert!(setup.room.pin_event(setup.event_id).await.unwrap().not());
}

#[async_test]
async fn test_pin_event_is_returning_an_error() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(false).await;

    // Pinning a remote event fails.
    setup.server.mock_set_room_pinned_events().unauthorized().mock_once().mount().await;

    assert!(setup.room.pin_event(setup.event_id).await.is_err());
}

#[async_test]
async fn test_unpin_event_is_sent_successfully() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(true).await;

    // Unpinning a remote event succeeds.
    setup.server.mock_set_room_pinned_events().ok(owned_event_id!("$42")).mock_once().mount().await;

    assert!(setup.room.unpin_event(setup.event_id).await.unwrap());
}

#[async_test]
async fn test_unpin_event_is_returning_false_because_is_not_pinned() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(false).await;

    assert!(setup.room.unpin_event(setup.event_id).await.unwrap().not());
}

#[async_test]
async fn test_unpin_event_is_returning_an_error() {
    let mut setup = PinningTestSetup::new().await;

    setup.mock_sync(true).await;

    // Unpinning a remote event fails.
    setup.server.mock_set_room_pinned_events().unauthorized().mock_once().mount().await;

    assert!(setup.room.unpin_event(setup.event_id).await.is_err());
}
