//! Tests for image packs: [`matrix_sdk::Room::image_packs`],
//! [`matrix_sdk::Room::available_image_packs`] and the related
//! [`matrix_sdk::Account`] methods.

use std::{collections::BTreeMap, pin::pin};

use matrix_sdk::{
    Client, Error, assert_next_with_timeout,
    room::image_packs::{AvailableImagePack, ImagePackSource},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, LeftRoomBuilder, async_test, event_factory::EventFactory,
};
use ruma::{
    OwnedMxcUri, OwnedRoomId, RoomId, RoomVersionId, event_id,
    events::{
        AnyGlobalAccountDataEvent, AnySyncStateEvent, StateEventType,
        image_pack::rooms::{ImagePackRoomsEventContent, RoomImagePackMeta},
        room::image_pack::{ImagePackImage, ImagePackMeta, RoomImagePackEventContent},
        space::parent::SpaceParentEventContent,
    },
    owned_room_id, room_id,
    serde::Raw,
    user_id,
};
use serde_json::{Value, json};
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

/// A summary of available packs, as `(room_id, state_key, source)` tuples.
fn summary(packs: &[AvailableImagePack]) -> Vec<(String, String, ImagePackSource)> {
    packs
        .iter()
        .map(|p| (p.pack.room_id.to_string(), p.pack.state_key.clone(), p.source.clone()))
        .collect()
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

/// Sync a joined room with the given image packs.
async fn sync_room_with_packs(
    server: &MatrixMockServer,
    client: &Client,
    room_id: &RoomId,
    packs: &[&str],
) -> matrix_sdk::Room {
    let f = EventFactory::new().room(room_id).sender(user_id!("@alice:example.org"));
    server
        .sync_room(
            client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(
                packs.iter().map(|state_key| f.image_pack(*state_key, pack(&["cat"])).into()),
            ),
        )
        .await
}

/// Sync a joined room with a parent space, which can be marked as canonical.
///
/// If `reciprocal` is true, the parent space also lists the room as a child, so
/// that the relationship can be verified.
async fn sync_room_with_parent(
    server: &MatrixMockServer,
    client: &Client,
    room_id: &RoomId,
    parent_id: &RoomId,
    canonical: bool,
    reciprocal: bool,
) {
    let f = EventFactory::new().room(room_id).sender(user_id!("@alice:example.org"));
    let mut content = SpaceParentEventContent::new(vec![]);
    content.canonical = canonical;
    server
        .sync_room(
            client,
            JoinedRoomBuilder::new(room_id)
                .add_state_event(f.event(content).state_key(parent_id.as_str())),
        )
        .await;

    if reciprocal {
        server
            .sync_room(
                client,
                JoinedRoomBuilder::new(parent_id)
                    .add_state_event(f.space_child(parent_id.to_owned(), room_id.to_owned())),
            )
            .await;
    }
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
async fn test_available_image_packs() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let space_id = room_id!("!space:example.org");
    let grand_space_id = room_id!("!grand_space:example.org");
    let other_room_id = room_id!("!other:example.org");
    let unknown_room_id = room_id!("!unknown:example.org");

    sync_room_with_packs(&server, &client, room_id, &["b", "a"]).await;
    sync_room_with_packs(&server, &client, other_room_id, &["global"]).await;
    sync_room_with_packs(&server, &client, space_id, &["space"]).await;
    sync_room_with_packs(&server, &client, grand_space_id, &["grand"]).await;

    sync_room_with_parent(&server, &client, room_id, space_id, true, true).await;
    sync_room_with_parent(&server, &client, space_id, grand_space_id, true, true).await;

    sync_image_pack_rooms(
        &server,
        &client,
        image_pack_rooms(&[
            (other_room_id, "global"),
            // This pack is also in the room itself. It must be returned only
            // once, as global.
            (room_id, "b"),
            // The user did not join this room, so the pack is skipped.
            (unknown_room_id, ""),
            // This pack does not exist, so it is skipped.
            (other_room_id, "missing"),
        ]),
    )
    .await;

    let room = client.get_room(room_id).unwrap();
    let packs = room.available_image_packs().await.unwrap();

    assert_eq!(
        summary(&packs),
        vec![
            (other_room_id.to_string(), "global".to_owned(), ImagePackSource::Global),
            (room_id.to_string(), "b".to_owned(), ImagePackSource::Global),
            (room_id.to_string(), "a".to_owned(), ImagePackSource::Room),
            (
                space_id.to_string(),
                "space".to_owned(),
                ImagePackSource::Space { space_id: space_id.to_owned() }
            ),
            (
                grand_space_id.to_string(),
                "grand".to_owned(),
                ImagePackSource::Space { space_id: grand_space_id.to_owned() }
            ),
        ]
    );
}

#[async_test]
async fn test_available_image_packs_space_cycle() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let space_a = room_id!("!space_a:example.org");
    let space_b = room_id!("!space_b:example.org");

    sync_room_with_packs(&server, &client, space_a, &["a"]).await;
    sync_room_with_packs(&server, &client, space_b, &["b"]).await;

    sync_room_with_parent(&server, &client, room_id, space_a, true, true).await;
    sync_room_with_parent(&server, &client, space_a, space_b, true, true).await;
    sync_room_with_parent(&server, &client, space_b, space_a, true, true).await;

    let room = client.get_room(room_id).unwrap();
    let packs = room.available_image_packs().await.unwrap();

    assert_eq!(
        summary(&packs),
        vec![
            (
                space_a.to_string(),
                "a".to_owned(),
                ImagePackSource::Space { space_id: space_a.to_owned() }
            ),
            (
                space_b.to_string(),
                "b".to_owned(),
                ImagePackSource::Space { space_id: space_b.to_owned() }
            ),
        ]
    );
}

#[async_test]
async fn test_available_image_packs_ignores_non_canonical_parent() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let space_id = room_id!("!space:example.org");

    sync_room_with_packs(&server, &client, space_id, &["space"]).await;
    sync_room_with_parent(&server, &client, room_id, space_id, false, true).await;

    let room = client.get_room(room_id).unwrap();
    assert!(room.available_image_packs().await.unwrap().is_empty());
}

#[async_test]
async fn test_available_image_packs_ignores_unverified_canonical_parent() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let space_id = room_id!("!space:example.org");

    // The space does not list the room as a child, and the power levels of the
    // space are unknown, so the relationship is not verified. The members of
    // the space must not be fetched from the server.
    sync_room_with_packs(&server, &client, space_id, &["space"]).await;
    sync_room_with_parent(&server, &client, room_id, space_id, true, false).await;
    server.mock_get_members().ok(vec![]).never().mount().await;

    let room = client.get_room(room_id).unwrap();
    assert!(room.available_image_packs().await.unwrap().is_empty());
}

#[async_test]
async fn test_available_image_packs_verifies_canonical_parent_with_power_levels() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let space_id = room_id!("!space:example.org");
    let other_space_id = room_id!("!other_space:example.org");
    let alice = user_id!("@alice:example.org");

    // Alice, the sender of the `m.space.parent` events, can send
    // `m.space.child` events in `space_id` but not in `other_space_id`, which
    // has the lowest room ID. Neither space lists the room as a child.
    sync_room_with_packs(&server, &client, space_id, &["space"]).await;
    sync_room_with_packs(&server, &client, other_space_id, &["other"]).await;

    for (id, alice_level) in [(space_id, 100), (other_space_id, 0)] {
        let f = EventFactory::new().room(id).sender(alice);
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(id).add_state_bulk([
                    f.create(user_id!("@creator:example.org"), RoomVersionId::V1).into(),
                    f.power_levels(&mut BTreeMap::from([(alice.to_owned(), alice_level.into())]))
                        .into(),
                ]),
            )
            .await;
    }

    sync_room_with_parent(&server, &client, room_id, space_id, true, false).await;
    sync_room_with_parent(&server, &client, room_id, other_space_id, true, false).await;
    server.mock_get_members().ok(vec![]).never().mount().await;

    let room = client.get_room(room_id).unwrap();
    assert_eq!(
        summary(&room.available_image_packs().await.unwrap()),
        vec![(
            space_id.to_string(),
            "space".to_owned(),
            ImagePackSource::Space { space_id: space_id.to_owned() }
        )]
    );
}

#[async_test]
async fn test_available_image_packs_in_invited_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let space_id = room_id!("!space:example.org");
    let f = EventFactory::new().room(room_id).sender(user_id!("@alice:example.org"));

    // The space lists the room as a child, so the relationship is verified.
    sync_room_with_packs(&server, &client, space_id, &["space"]).await;
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(space_id)
                .add_state_event(f.space_child(space_id.to_owned(), room_id.to_owned())),
        )
        .await;

    // The state of an invited room is stripped.
    let mut parent = SpaceParentEventContent::new(vec![]);
    parent.canonical = true;
    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id)
                .add_state_event(f.image_pack("invite", pack(&["cat"])))
                .add_state_event(f.event(parent).state_key(space_id.as_str())),
        )
        .await;

    assert_eq!(
        summary(&room.available_image_packs().await.unwrap()),
        vec![
            (room_id.to_string(), "invite".to_owned(), ImagePackSource::Room),
            (
                space_id.to_string(),
                "space".to_owned(),
                ImagePackSource::Space { space_id: space_id.to_owned() }
            ),
        ]
    );
}

#[async_test]
async fn test_available_image_packs_ignores_left_canonical_space() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let space_id = room_id!("!space:example.org");

    sync_room_with_packs(&server, &client, space_id, &["space"]).await;
    sync_room_with_parent(&server, &client, room_id, space_id, true, true).await;
    server.sync_room(&client, LeftRoomBuilder::new(space_id)).await;

    let room = client.get_room(room_id).unwrap();
    assert!(room.available_image_packs().await.unwrap().is_empty());
}

#[async_test]
async fn test_available_image_packs_ignores_malformed_space_events() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let malformed_child_space_id = room_id!("!a_space:example.org");
    let redacted_child_space_id = room_id!("!b_space:example.org");

    let state_event = |room_id: &RoomId, event_type: &str, state_key: &str, extra: Value| {
        let mut event = json!({
            "type": event_type,
            "state_key": state_key,
            "event_id": format!("$event_{state_key}"),
            "sender": "@alice:example.org",
            "origin_server_ts": 1,
            "room_id": room_id,
        });
        event.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        Raw::new(&event).unwrap().cast_unchecked::<AnySyncStateEvent>()
    };

    // The `m.space.child` event of the first space is malformed, and the one of
    // the second space is redacted, so neither relationship is verified.
    sync_room_with_packs(&server, &client, malformed_child_space_id, &["malformed"]).await;
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(malformed_child_space_id).add_state_event(state_event(
                malformed_child_space_id,
                "m.space.child",
                room_id.as_str(),
                json!({ "content": { "via": "not an array" } }),
            )),
        )
        .await;

    sync_room_with_packs(&server, &client, redacted_child_space_id, &["redacted"]).await;
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(redacted_child_space_id).add_state_event(state_event(
                redacted_child_space_id,
                "m.space.child",
                room_id.as_str(),
                json!({
                    "content": {},
                    "unsigned": {
                        "redacted_because": {
                            "type": "m.room.redaction",
                            "event_id": "$redaction",
                            "sender": "@alice:example.org",
                            "origin_server_ts": 2,
                            "redacts": format!("$event_{room_id}"),
                            "content": {},
                        },
                    },
                }),
            )),
        )
        .await;

    // The first `m.space.parent` event is malformed, because its state key is
    // not a room ID.
    let canonical = json!({ "content": { "via": ["example.org"], "canonical": true } });
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk([
                state_event(room_id, "m.space.parent", "not_a_room_id", canonical.clone()),
                state_event(
                    room_id,
                    "m.space.parent",
                    malformed_child_space_id.as_str(),
                    canonical.clone(),
                ),
                state_event(room_id, "m.space.parent", redacted_child_space_id.as_str(), canonical),
            ]),
        )
        .await;

    assert!(room.available_image_packs().await.unwrap().is_empty());
}

#[async_test]
async fn test_available_image_packs_with_malformed_image_pack_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!room:example.org");
    let room = sync_room_with_packs(&server, &client, room_id, &["cats"]).await;

    // `not_a_room_id` is not a valid room ID, so the entry is ignored.
    let raw: Raw<AnyGlobalAccountDataEvent> = Raw::new(&json!({
        "type": "m.image_pack.rooms",
        "content": { "rooms": { "not_a_room_id": { "": {} } } },
    }))
    .unwrap()
    .cast_unchecked();
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(raw);
        })
        .await;

    // The malformed account data is ignored. The packs of the room itself are
    // still available.
    assert!(client.account().image_pack_rooms().await.unwrap().rooms.is_empty());
    assert_eq!(
        summary(&room.available_image_packs().await.unwrap()),
        vec![(room_id.to_string(), "cats".to_owned(), ImagePackSource::Room)]
    );
}

#[async_test]
async fn test_enable_image_pack_globally_with_malformed_image_pack_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!room:example.org");

    let other_room_id = room_id!("!other:example.org");

    // The invalid entries are dropped and the valid ones are kept, so the user
    // can repair the event.
    server
        .mock_get_image_pack_rooms()
        .ok(
            user_id,
            &json!({ "rooms": {
                "not_a_room_id": { "": {} },
                other_room_id: { "dogs": {}, "birds": "not an object" },
            } }),
        )
        .mount()
        .await;
    server
        .mock_set_image_pack_rooms()
        .match_content(&image_pack_rooms(&[(other_room_id, "dogs"), (room_id, "cats")]))
        .ok(user_id)
        .mock_once()
        .mount()
        .await;

    client.account().enable_image_pack_globally(room_id, "cats").await.unwrap();
}

#[async_test]
async fn test_enable_and_disable_image_pack_globally() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let account = client.account();

    let room_id = room_id!("!room:example.org");
    let other_room_id = owned_room_id!("!other:example.org");

    // Enabling a pack without existing account data creates it.
    {
        let _get = server.mock_get_image_pack_rooms().not_found(user_id).mount_as_scoped().await;
        let _set = server
            .mock_set_image_pack_rooms()
            .match_content(&image_pack_rooms(&[(room_id, "cats")]))
            .ok(user_id)
            .mock_once()
            .mount_as_scoped()
            .await;

        account.enable_image_pack_globally(room_id, "cats").await.unwrap();

        // Storage is updated before the sync echo arrives.
        assert_eq!(
            json!(account.image_pack_rooms().await.unwrap()),
            json!(image_pack_rooms(&[(room_id, "cats")]))
        );
    }

    // If the pack is already enabled, enabling it does not upload anything.
    {
        let _get = server
            .mock_get_image_pack_rooms()
            .ok(user_id, &image_pack_rooms(&[(room_id, "cats")]))
            .mount_as_scoped()
            .await;
        let _set = server.mock_set_image_pack_rooms().ok(user_id).never().mount_as_scoped().await;

        account.enable_image_pack_globally(room_id, "cats").await.unwrap();
    }

    // Disabling the last pack of a room removes the room.
    {
        let _get = server
            .mock_get_image_pack_rooms()
            .ok(user_id, &image_pack_rooms(&[(room_id, "cats"), (&other_room_id, "")]))
            .mount_as_scoped()
            .await;
        let _set = server
            .mock_set_image_pack_rooms()
            .match_content(&image_pack_rooms(&[(&other_room_id, "")]))
            .ok(user_id)
            .mock_once()
            .mount_as_scoped()
            .await;

        account.disable_image_pack_globally(room_id, "cats").await.unwrap();

        assert_eq!(
            json!(account.image_pack_rooms().await.unwrap()),
            json!(image_pack_rooms(&[(&other_room_id, "")]))
        );
    }

    // If the pack is not enabled, disabling it does not upload anything.
    {
        let _get = server
            .mock_get_image_pack_rooms()
            .ok(user_id, &image_pack_rooms(&[(&other_room_id, "")]))
            .mount_as_scoped()
            .await;
        let _set = server.mock_set_image_pack_rooms().ok(user_id).never().mount_as_scoped().await;

        account.disable_image_pack_globally(room_id, "cats").await.unwrap();
    }
}

#[async_test]
async fn test_enable_and_disable_image_pack_globally_keep_unknown_fields() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let account = client.account();

    let room_id = room_id!("!room:example.org");
    let other_room_id = room_id!("!other:example.org");

    // Fields unknown to this client, possibly written by other clients, must
    // be kept when the event is updated.
    let existing = json!({
        "rooms": { other_room_id: { "dogs": { "x.custom": 1 } } },
        "x.top_level": true,
    });

    {
        let _get =
            server.mock_get_image_pack_rooms().ok(user_id, &existing).mount_as_scoped().await;
        let _set = server
            .mock_set_image_pack_rooms()
            .match_content(&json!({
                "rooms": {
                    other_room_id: { "dogs": { "x.custom": 1 } },
                    room_id: { "cats": {} },
                },
                "x.top_level": true,
            }))
            .ok(user_id)
            .mock_once()
            .mount_as_scoped()
            .await;

        account.enable_image_pack_globally(room_id, "cats").await.unwrap();
    }

    {
        let _get = server
            .mock_get_image_pack_rooms()
            .ok(
                user_id,
                &json!({
                    "rooms": {
                        other_room_id: { "dogs": { "x.custom": 1 } },
                        room_id: { "cats": {} },
                    },
                    "x.top_level": true,
                }),
            )
            .mount_as_scoped()
            .await;
        let _set = server
            .mock_set_image_pack_rooms()
            .match_content(&existing)
            .ok(user_id)
            .mock_once()
            .mount_as_scoped()
            .await;

        account.disable_image_pack_globally(room_id, "cats").await.unwrap();
    }

    // Storage is updated with the uploaded content.
    assert_eq!(
        json!(account.image_pack_rooms().await.unwrap()),
        json!(image_pack_rooms(&[(other_room_id, "dogs")]))
    );
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
