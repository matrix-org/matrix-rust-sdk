//! Tests for [`matrix_sdk_ui::image_packs`].

use std::collections::BTreeMap;

use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_test::{
    DEFAULT_TEST_ROOM_ID, JoinedRoomBuilder, LeftRoomBuilder, User, async_test,
    event_factory::EventFactory,
};
use matrix_sdk_ui::{image_packs::image_packs, spaces::SpaceService};
use ruma::{
    RoomId, RoomVersionId,
    events::{
        image_pack::rooms::{ImagePackRoomsEventContent, RoomImagePackMeta},
        room::image_pack::{ImagePackImage, RoomImagePackEventContent},
    },
    owned_mxc_uri, owned_room_id,
};

/// A pack with a single image, named after the pack.
fn pack(name: &str) -> RoomImagePackEventContent {
    RoomImagePackEventContent::new(BTreeMap::from([(
        name.to_owned(),
        ImagePackImage::new(owned_mxc_uri!("mxc://localhost/image")),
    )]))
}

/// A [`pack`] with a display name.
fn named_pack(name: &str, display_name: &str) -> RoomImagePackEventContent {
    let mut content = pack(name);
    content.pack.display_name = Some(display_name.to_owned());
    content
}

#[async_test]
async fn test_image_packs() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let other_room_id = owned_room_id!("!other:localhost");
    let left_room_id = owned_room_id!("!left:localhost");
    let space_id = owned_room_id!("!space:localhost");
    let parent_space_id = owned_room_id!("!parent_space:localhost");
    let f = EventFactory::new().sender(User::Alice);

    // The user enabled a pack of this room, a pack of another room, and a pack
    // of a room they left.
    let enabled = ImagePackRoomsEventContent::new(BTreeMap::from([
        (
            DEFAULT_TEST_ROOM_ID.to_owned(),
            BTreeMap::from([("local".to_owned(), RoomImagePackMeta::new())]),
        ),
        (other_room_id.clone(), BTreeMap::from([("global".to_owned(), RoomImagePackMeta::new())])),
        (left_room_id.clone(), BTreeMap::from([("gone".to_owned(), RoomImagePackMeta::new())])),
    ]));

    // The room is in a space, which is itself in a space, each with a pack.
    let space = |space_id: &RoomId, child_id, pack_name: &str| {
        JoinedRoomBuilder::new(space_id)
            .add_state_event(f.create(User::Alice, RoomVersionId::V11).with_space_type())
            .add_state_event(f.space_child(space_id.to_owned(), child_id))
            .add_state_event(f.event(pack(pack_name)).state_key(pack_name))
    };

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_global_account_data(f.event(enabled))
                .add_joined_room(
                    JoinedRoomBuilder::default()
                        .add_state_event(f.room_name("Emojis").default_room())
                        .add_state_event(f.event(pack("local")).default_room().state_key("local"))
                        .add_state_event(
                            f.event(RoomImagePackEventContent::new(BTreeMap::new()))
                                .default_room()
                                .state_key("empty"),
                        )
                        .add_state_event(
                            f.event(named_pack("room_only", "Room only"))
                                .default_room()
                                .state_key("room_only"),
                        ),
                )
                .add_joined_room(
                    JoinedRoomBuilder::new(&other_room_id)
                        .add_state_event(f.event(pack("global")).state_key("global"))
                        .add_state_event(f.event(pack("other")).state_key("other")),
                )
                .add_left_room(
                    LeftRoomBuilder::new(&left_room_id)
                        .add_state_bulk([f.event(pack("gone")).state_key("gone").into()]),
                )
                .add_joined_room(space(&space_id, DEFAULT_TEST_ROOM_ID.to_owned(), "space"))
                .add_joined_room(space(&parent_space_id, space_id.clone(), "parent_space"));
        })
        .await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    let space_service = SpaceService::new(client.clone()).await;
    let packs = image_packs(&room, &space_service).await.unwrap();

    // The enabled packs come first, then the ones of the room that aren't
    // enabled already, then the ones of its spaces, closest first. The packs
    // of the left room and the ones of the other room that aren't enabled are
    // skipped, as is the empty pack. The packs without a name use the name of
    // their room, when it's known.
    let packs = packs
        .iter()
        .map(|pack| {
            (
                pack.room_id.as_str(),
                pack.state_key.as_str(),
                pack.content.images.keys().next().unwrap().as_str(),
                pack.content.pack.display_name.as_deref(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        packs,
        [
            (DEFAULT_TEST_ROOM_ID.as_str(), "local", "local", Some("Emojis")),
            (other_room_id.as_str(), "global", "global", None),
            (DEFAULT_TEST_ROOM_ID.as_str(), "room_only", "room_only", Some("Room only")),
            (space_id.as_str(), "space", "space", None),
            (parent_space_id.as_str(), "parent_space", "parent_space", None),
        ]
    );
}
