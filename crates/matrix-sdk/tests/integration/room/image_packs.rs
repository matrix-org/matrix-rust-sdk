//! Tests for image packs: [`matrix_sdk::Room::image_packs`] and the related
//! [`matrix_sdk::Account`] methods.

use std::{collections::BTreeMap, pin::pin};

use matrix_sdk::{Client, Error, assert_next_with_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{
    OwnedMxcUri, OwnedRoomId, RoomId, event_id,
    events::{
        AnySyncStateEvent, StateEventType,
        image_pack::rooms::{ImagePackRoomsEventContent, RoomImagePackMeta},
        room::image_pack::{ImagePackImage, ImagePackMeta, RoomImagePackEventContent},
    },
    room_id,
    serde::Raw,
    user_id,
};
use serde_json::json;
use strass::assert_let;
use stream_assert::assert_pending;

/// An image pack containing an image for each of the given shortcodes.
fn pack(shortcodes: &[&str]) -> RoomImagePackEventContent {
    RoomImagePackEventContent::new(
        shortcodes
            .iter()
            .map(|shortcode| {
                let url = OwnedMxcUri::from(format!("mxc://example.org/{shortcode}"));
                (shortcode.to_string(), ImagePackImage::new(url))
            })
            .collect(),
    )
}

/// The content of an `m.image_pack.rooms` event referencing the given packs.
fn image_pack_rooms(packs: &[(&RoomId, &str)]) -> ImagePackRoomsEventContent {
    let mut rooms = BTreeMap::<OwnedRoomId, BTreeMap<String, RoomImagePackMeta>>::new();
    for (room_id, state_key) in packs {
        rooms
            .entry((*room_id).to_owned())
            .or_default()
            .insert((*state_key).to_owned(), RoomImagePackMeta::new());
    }
    ImagePackRoomsEventContent::new(rooms)
}

/// Sync the `m.image_pack.rooms` global account data.
async fn sync_image_pack_rooms(
    server: &MatrixMockServer,
    client: &Client,
    content: ImagePackRoomsEventContent,
) {
    let f = EventFactory::new();
    server
        .mock_sync()
        .ok_and_run(client, |builder| {
            builder.add_global_account_data(f.image_pack_rooms(content));
        })
        .await;
}

#[async_test]
async fn test_image_packs() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let f = EventFactory::new().room(room_id).sender(user_id!("@alice:example.org"));

    let mut cats = pack(&["cat_wave", "cat_nap"]);
    cats.pack = ImagePackMeta::new();
    cats.pack.display_name = Some("Cats".to_owned());

    let redacted: Raw<AnySyncStateEvent> = Raw::new(&json!({
        "type": "m.room.image_pack",
        "state_key": "redacted",
        "event_id": "$redacted",
        "sender": "@alice:example.org",
        "origin_server_ts": 1,
        "content": {},
        "unsigned": {
            "redacted_because": {
                "type": "m.room.redaction",
                "event_id": "$redaction",
                "sender": "@alice:example.org",
                "origin_server_ts": 2,
                "redacts": "$redacted",
                "content": {},
            },
        },
    }))
    .unwrap()
    .cast_unchecked();

    // Missing the required `images` field.
    let invalid: Raw<AnySyncStateEvent> = Raw::new(&json!({
        "type": "m.room.image_pack",
        "state_key": "invalid",
        "event_id": "$invalid",
        "sender": "@alice:example.org",
        "origin_server_ts": 1,
        "content": { "pack": { "display_name": "Invalid" } },
    }))
    .unwrap()
    .cast_unchecked();

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_event(f.image_pack("", pack(&["dog"])))
                .add_state_event(f.image_pack("cats", cats))
                .add_state_bulk([redacted, invalid]),
        )
        .await;

    let mut packs = room.image_packs().await.unwrap();
    packs.sort_by(|a, b| a.state_key.cmp(&b.state_key));

    assert_eq!(packs.len(), 2);
    assert_eq!(packs[0].room_id, room_id);
    assert_eq!(packs[0].state_key, "");
    assert!(packs[0].content.images.contains_key("dog"));
    assert_eq!(packs[1].state_key, "cats");
    assert_eq!(packs[1].content.pack.display_name.as_deref(), Some("Cats"));
    assert_eq!(packs[1].content.images.len(), 2);

    let cats = room.image_pack("cats").await.unwrap().unwrap();
    assert!(cats.content.images.contains_key("cat_nap"));
    assert!(room.image_pack("redacted").await.unwrap().is_none());
    assert!(room.image_pack("invalid").await.unwrap().is_none());
    assert!(room.image_pack("missing").await.unwrap().is_none());
}

#[async_test]
async fn test_set_image_pack() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server
        .mock_room_send_state()
        .for_type(StateEventType::RoomImagePack)
        .for_key("cats".to_owned())
        .body_matches_partial_json(json!({
            "images": { "cat_wave": { "url": "mxc://example.org/cat_wave" } },
        }))
        .ok(event_id!("$pack"))
        .mock_once()
        .mount()
        .await;

    let response = room.set_image_pack("cats", pack(&["cat_wave"])).await.unwrap();
    assert_eq!(response.event_id, "$pack");

    // The method rejects a shortcode that is not valid before it sends a
    // request.
    let result = room.set_image_pack("cats", pack(&["cat:wave"])).await;
    assert_let!(Err(Error::InvalidImagePackShortcode(shortcode)) = result);
    assert_eq!(shortcode, "cat:wave");
}

#[async_test]
async fn test_observe_image_pack_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let account = client.account();

    let room_id = room_id!("!room:example.org");

    assert!(account.image_pack_rooms().await.unwrap().rooms.is_empty());

    let (initial, stream) = account.observe_image_pack_rooms().await.unwrap();
    assert!(initial.is_none());
    let mut stream = pin!(stream);
    assert_pending!(stream);

    let content = image_pack_rooms(&[(room_id, "cats")]);
    sync_image_pack_rooms(&server, &client, content.clone()).await;

    let update = assert_next_with_timeout!(stream);
    assert_eq!(json!(update), json!(content));
    assert_eq!(json!(account.image_pack_rooms().await.unwrap()), json!(content));

    let (initial, _stream) = account.observe_image_pack_rooms().await.unwrap();
    assert_eq!(json!(initial.unwrap()), json!(content));
}
