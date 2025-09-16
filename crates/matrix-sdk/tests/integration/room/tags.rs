use std::{collections::BTreeMap, ops::Not, time::Duration};

use matrix_sdk::{
    Client, Room,
    config::{SyncSettings, SyncToken},
};
use matrix_sdk_test::{
    JoinedRoomBuilder, RoomAccountDataTestEvent, SyncResponseBuilder, async_test, test_json,
};
use ruma::{
    RoomId,
    events::tag::{TagInfo, TagName, Tags},
    room_id,
};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path_regex},
};

use crate::{logged_in_client_with_server, mock_sync};

enum TagOperation {
    Set,
    Remove,
}

async fn mock_tag_api(
    server: &MockServer,
    tag_name: TagName,
    tag_operation: TagOperation,
    expect: u64,
) {
    let method = match tag_operation {
        TagOperation::Set => method("PUT"),
        TagOperation::Remove => method("DELETE"),
    };
    let path_regex_str =
        format!(r"^/_matrix/client/r0/user/.*/rooms/.*/tags/{}", tag_name.as_ref());
    Mock::given(method)
        .and(path_regex(path_regex_str))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .expect(expect)
        .mount(server)
        .await;
}

async fn mock_sync_with_tags(
    server: &MockServer,
    sync_builder: &mut SyncResponseBuilder,
    room_id: &RoomId,
    tags: Tags,
) {
    let json = json!({
        "content": {
            "tags": tags,
        },
        "type": "m.tag"
    });
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_account_data(RoomAccountDataTestEvent::Custom(json)),
    );
    mock_sync(server, sync_builder.build_json_sync_response(), None).await;
}

async fn sync_once(client: &Client, server: &MockServer) {
    let sync_settings =
        SyncSettings::new().timeout(Duration::from_millis(3000)).token(SyncToken::NoToken);
    client.sync_once(sync_settings).await.unwrap();
    server.reset().await;
}

async fn synced_client_with_room(
    sync_builder: &mut SyncResponseBuilder,
    room_id: &RoomId,
) -> (Client, Room, MockServer) {
    let (client, server) = logged_in_client_with_server().await;
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    sync_once(&client, &server).await;

    let room = client.get_room(room_id).unwrap();
    (client, room, server)
}

#[async_test]
async fn test_set_favourite() {
    let room_id = room_id!("!test:example.org");
    let mut sync_builder = SyncResponseBuilder::new();
    let (client, room, server) = synced_client_with_room(&mut sync_builder, room_id).await;

    // Not favourite.
    assert!(room.is_favourite().not());

    // Server will be called to set the room as favourite.
    mock_tag_api(&server, TagName::Favorite, TagOperation::Set, 1).await;

    room.set_is_favourite(true, None).await.unwrap();

    // Mock the response from the server.
    let tags = BTreeMap::from([(TagName::Favorite, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut sync_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    // Favourite!
    assert!(room.is_favourite());

    server.verify().await;
}

#[async_test]
async fn test_set_favourite_on_low_priority_room() {
    let room_id = room_id!("!test:example.org");
    let mut sync_builder = SyncResponseBuilder::new();
    let (client, room, server) = synced_client_with_room(&mut sync_builder, room_id).await;

    // Mock a response from the server setting the room as low priority.
    let tags = BTreeMap::from([(TagName::LowPriority, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut sync_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    // Not favourite, but low priority!
    assert!(room.is_favourite().not());
    assert!(room.is_low_priority());

    // Server will be called to set the room as favourite, and to unset the room as
    // low priority.
    mock_tag_api(&server, TagName::Favorite, TagOperation::Set, 1).await;
    mock_tag_api(&server, TagName::LowPriority, TagOperation::Remove, 1).await;

    room.set_is_favourite(true, None).await.unwrap();

    // Mock the response from the server.
    let tags = BTreeMap::from([(TagName::Favorite, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut sync_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    // Favourite, and not low priority!
    assert!(room.is_favourite());
    assert!(room.is_low_priority().not());

    server.verify().await;
}

#[async_test]
async fn test_unset_favourite() {
    let room_id = room_id!("!test:example.org");
    let mut sync_builder = SyncResponseBuilder::new();
    let (_client, room, server) = synced_client_with_room(&mut sync_builder, room_id).await;

    // Server will be called to unset the room as favourite.
    mock_tag_api(&server, TagName::Favorite, TagOperation::Remove, 1).await;

    room.set_is_favourite(false, None).await.unwrap();

    server.verify().await;
}

#[async_test]
async fn test_set_low_priority() {
    let room_id = room_id!("!test:example.org");
    let mut sync_builder = SyncResponseBuilder::new();
    let (client, room, server) = synced_client_with_room(&mut sync_builder, room_id).await;

    // Not low prioriry.
    assert!(room.is_low_priority().not());

    // Server will be called to set the room as favourite.
    mock_tag_api(&server, TagName::LowPriority, TagOperation::Set, 1).await;

    room.set_is_low_priority(true, None).await.unwrap();

    // Mock the response from the server.
    let tags = BTreeMap::from([(TagName::LowPriority, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut sync_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    // Low priority!
    assert!(room.is_low_priority());

    server.verify().await;
}

#[async_test]
async fn test_set_low_priority_on_favourite_room() {
    let room_id = room_id!("!test:example.org");
    let mut sync_builder = SyncResponseBuilder::new();
    let (client, room, server) = synced_client_with_room(&mut sync_builder, room_id).await;

    // Mock a response from the server setting the room as favourite.
    let tags = BTreeMap::from([(TagName::Favorite, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut sync_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    // Favourite, but not low priority!
    assert!(room.is_favourite());
    assert!(room.is_low_priority().not());

    // Server will be called to set the room as favourite, and to unset the room as
    // low priority.
    mock_tag_api(&server, TagName::LowPriority, TagOperation::Set, 1).await;
    mock_tag_api(&server, TagName::Favorite, TagOperation::Remove, 1).await;

    room.set_is_low_priority(true, None).await.unwrap();

    // Mock the response from the server.
    let tags = BTreeMap::from([(TagName::LowPriority, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut sync_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    // Not favourite, and low priority!
    assert!(room.is_favourite().not());
    assert!(room.is_low_priority());

    server.verify().await;
}

#[async_test]
async fn test_unset_low_priority() {
    let room_id = room_id!("!test:example.org");
    let mut sync_builder = SyncResponseBuilder::new();
    let (_client, room, server) = synced_client_with_room(&mut sync_builder, room_id).await;

    // Server will be called to unset the room as favourite.
    mock_tag_api(&server, TagName::LowPriority, TagOperation::Remove, 1).await;

    room.set_is_low_priority(false, None).await.unwrap();

    server.verify().await;
}
