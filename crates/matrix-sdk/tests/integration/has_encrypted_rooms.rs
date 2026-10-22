use std::time::Duration;

use assert_matches::assert_matches;
use matrix_sdk::{
    Error, sliding_sync::Version as SlidingSyncVersion, test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{RoomVersionId, UInt, api::client::sync::sync_events::v5, assign, room_id, user_id};
use serde_json::json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_partial_json, method, path, query_param},
};

fn encrypted_rooms_count_response(count: u32) -> v5::Response {
    let mut response = v5::Response::new("pos".to_owned());
    response.lists.insert(
        "encrypted_rooms".to_owned(),
        assign!(v5::response::List::default(), { count: UInt::from(count) }),
    );
    response
}

async fn new_server_and_client() -> (MatrixMockServer, matrix_sdk::Client) {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    client.set_sliding_sync_version(SlidingSyncVersion::Native);

    (server, client)
}

#[async_test]
async fn test_has_encrypted_rooms_with_an_encrypted_room() {
    let (server, client) = new_server_and_client().await;

    server.mock_sliding_sync().ok(encrypted_rooms_count_response(1)).mock_once().mount().await;

    assert!(client.has_encrypted_rooms(Duration::from_secs(10)).await.unwrap());
}

#[async_test]
async fn test_has_encrypted_rooms_without_any_encrypted_room() {
    let (server, client) = new_server_and_client().await;

    server.mock_sliding_sync().ok(encrypted_rooms_count_response(0)).mock_once().mount().await;

    assert!(!client.has_encrypted_rooms(Duration::from_secs(10)).await.unwrap());
}

#[async_test]
async fn test_has_encrypted_rooms_sends_a_minimal_filtered_request() {
    let (server, client) = new_server_and_client().await;

    // Only answer a request carrying exactly the expected list and filters, to
    // ensure the client doesn't send a full sync request.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"))
        // The one-off request must not mark the user as online.
        .and(query_param("set_presence", "offline"))
        .and(body_partial_json(json!({
            "lists": {
                "encrypted_rooms": {
                    "ranges": [[0, 0]],
                    "timeline_limit": 0,
                    "filters": {
                        "is_encrypted": true,
                        "is_invite": false,
                        "not_room_types": ["m.space"],
                    },
                },
            },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pos": "pos",
            "lists": { "encrypted_rooms": { "count": 0 } },
            "rooms": {},
            "extensions": {},
        })))
        .expect(1)
        .mount(server.server())
        .await;

    assert!(!client.has_encrypted_rooms(Duration::from_secs(10)).await.unwrap());
}

#[async_test]
async fn test_has_encrypted_rooms_knows_cached_encrypted_rooms() {
    let (server, client) = new_server_and_client().await;

    // A joined room with a known `m.room.encryption` state.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!encrypted:bar.org")).add_state_event(
                EventFactory::new().sender(user_id!("@example:bar.org")).room_encryption(),
            ),
        )
        .await;

    // Deliberately do not mock the sliding sync endpoint to ensure the answer
    // comes from the local cache.
    assert!(client.has_encrypted_rooms(Duration::from_secs(10)).await.unwrap());
}

#[async_test]
async fn test_has_encrypted_rooms_counts_unknown_encryption_as_encrypted() {
    let (server, client) = new_server_and_client().await;

    // This room comes from a regular sync, which doesn't tell whether the room
    // is encrypted or not, so its encryption state is unknown.
    let room = server.sync_joined_room(&client, room_id!("!unknown:bar.org")).await;
    assert!(room.encryption_state().is_unknown());

    // Do not mock the sliding sync endpoint to make sure an unknown encryption
    // state counts as encrypted.
    assert!(client.has_encrypted_rooms(Duration::from_secs(10)).await.unwrap());
}

#[async_test]
async fn test_has_encrypted_rooms_ignores_spaces_in_the_cache() {
    let (server, client) = new_server_and_client().await;

    // Assert that spaces are ignored in the cache, and that the server is asked
    // for an authoritative count that does not include spaces.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!space:bar.org")).add_state_event(
                EventFactory::new()
                    .create(user_id!("@example:bar.org"), RoomVersionId::V11)
                    .with_space_type(),
            ),
        )
        .await;
    assert!(room.is_space());

    server.mock_sliding_sync().ok(encrypted_rooms_count_response(0)).mock_once().mount().await;

    assert!(!client.has_encrypted_rooms(Duration::from_secs(10)).await.unwrap());
}

#[async_test]
async fn test_has_encrypted_rooms_times_out() {
    let (server, client) = new_server_and_client().await;

    // The server takes longer to answer than the caller is willing to wait.
    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "pos": "pos",
                    "lists": { "encrypted_rooms": { "count": 1 } },
                    "rooms": {},
                    "extensions": {},
                }))
                .set_delay(Duration::from_secs(30)),
        )
        .mount(server.server())
        .await;

    let result = client.has_encrypted_rooms(Duration::from_millis(100)).await;

    assert_matches!(result, Err(Error::Timeout));
}
