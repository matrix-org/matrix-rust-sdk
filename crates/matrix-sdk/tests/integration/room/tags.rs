use std::{collections::BTreeMap, time::Duration};

use matrix_sdk::{config::SyncSettings, Client, Room};
use matrix_sdk_test::{
    async_test, test_json, JoinedRoomBuilder, RoomAccountDataTestEvent, SyncResponseBuilder,
};
use ruma::{
    events::tag::{TagInfo, TagName, Tags},
    room_id, RoomId,
};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, MockServer, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync};

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
    ev_builder: &mut SyncResponseBuilder,
    room_id: &RoomId,
    tags: Tags,
) {
    let json = json!({
        "content": {
            "tags": tags,
        },
        "type": "m.tag"
    });
    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_account_data(RoomAccountDataTestEvent::Custom(json)),
    );
    mock_sync(server, ev_builder.build_json_sync_response(), None).await;
}

async fn sync_once(client: &Client, server: &MockServer) {
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    client.sync_once(sync_settings).await.unwrap();
    server.reset().await;
}

async fn synced_client_with_room(
    ev_builder: &mut SyncResponseBuilder,
    room_id: &RoomId,
) -> (Client, Room, MockServer) {
    let (client, server) = logged_in_client().await;
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    sync_once(&client, &server).await;

    let room = client.get_room(room_id).unwrap();
    (client, room, server)
}

#[async_test]
async fn when_set_is_favorite_is_run_with_true_then_set_tag_api_is_called() {
    let room_id = room_id!("!test:example.org");
    let mut ev_builder = SyncResponseBuilder::new();
    let (_client, room, server) = synced_client_with_room(&mut ev_builder, room_id).await;

    mock_tag_api(&server, TagName::Favorite, TagOperation::Set, 1).await;

    room.set_is_favorite(true, Option::default()).await.unwrap();

    server.verify().await;
}

#[async_test]
async fn when_set_is_favorite_is_run_with_true_and_low_priority_tag_was_set_then_set_tag_and_remove_tag_apis_are_called(
) {
    let room_id = room_id!("!test:example.org");
    let mut ev_builder = SyncResponseBuilder::new();
    let (client, room, server) = synced_client_with_room(&mut ev_builder, room_id).await;
    let tags = BTreeMap::from([(TagName::LowPriority, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut ev_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    mock_tag_api(&server, TagName::Favorite, TagOperation::Set, 1).await;
    mock_tag_api(&server, TagName::LowPriority, TagOperation::Remove, 1).await;

    room.set_is_favorite(true, Option::default()).await.unwrap();

    server.verify().await;
}

#[async_test]
async fn when_set_is_favorite_is_run_with_false_then_delete_tag_api_is_called() {
    let room_id = room_id!("!test:example.org");
    let mut ev_builder = SyncResponseBuilder::new();
    let (_client, room, server) = synced_client_with_room(&mut ev_builder, room_id).await;

    mock_tag_api(&server, TagName::Favorite, TagOperation::Remove, 1).await;

    room.set_is_favorite(false, Option::default()).await.unwrap();

    server.verify().await;
}

#[async_test]
async fn when_set_is_low_priority_is_run_with_true_then_set_tag_api_is_called() {
    let room_id = room_id!("!test:example.org");
    let mut ev_builder = SyncResponseBuilder::new();
    let (_client, room, server) = synced_client_with_room(&mut ev_builder, room_id).await;

    mock_tag_api(&server, TagName::LowPriority, TagOperation::Set, 1).await;

    room.set_is_low_priority(true, Option::default()).await.unwrap();

    server.verify().await;
}

#[async_test]
async fn when_set_is_low_priority_is_run_with_true_and_favorite_tag_was_set_then_set_tag_and_remove_tag_apis_are_called(
) {
    let room_id = room_id!("!test:example.org");
    let mut ev_builder = SyncResponseBuilder::new();
    let (client, room, server) = synced_client_with_room(&mut ev_builder, room_id).await;
    let tags = BTreeMap::from([(TagName::Favorite, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut ev_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    mock_tag_api(&server, TagName::LowPriority, TagOperation::Set, 1).await;
    mock_tag_api(&server, TagName::Favorite, TagOperation::Remove, 1).await;

    room.set_is_low_priority(true, Option::default()).await.unwrap();

    server.verify().await;
}

#[async_test]
async fn when_set_is_low_priority_is_run_with_false_then_delete_tag_api_is_called() {
    let room_id = room_id!("!test:example.org");
    let mut ev_builder = SyncResponseBuilder::new();
    let (_client, room, server) = synced_client_with_room(&mut ev_builder, room_id).await;

    mock_tag_api(&server, TagName::LowPriority, TagOperation::Remove, 1).await;

    room.set_is_low_priority(false, Option::default()).await.unwrap();

    server.verify().await;
}

#[async_test]
async fn when_favorite_tag_is_set_in_sync_response_then_notable_tags_is_favorite_is_true() {
    let room_id = room_id!("!test:example.org");
    let mut ev_builder = SyncResponseBuilder::new();
    let (client, room, server) = synced_client_with_room(&mut ev_builder, room_id).await;

    let (initial_notable_tags, mut notable_tags_stream) = room.notable_tags_stream().await;

    assert!(!initial_notable_tags.is_favorite);

    // Ensure the notable tags stream is updated when the favorite tag is set on
    // sync
    let tags = BTreeMap::from([(TagName::Favorite, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut ev_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    assert!(notable_tags_stream.next().await.unwrap().is_favorite);
}

#[async_test]
async fn when_low_priority_tag_is_set_in_sync_response_then_notable_tags_is_low_priority_is_true() {
    let room_id = room_id!("!test:example.org");
    let mut ev_builder = SyncResponseBuilder::new();
    let (client, room, server) = synced_client_with_room(&mut ev_builder, room_id).await;

    let (initial_notable_tags, mut notable_tags_stream) = room.notable_tags_stream().await;

    assert!(!initial_notable_tags.is_low_priority);

    // Ensure the notable tags stream is updated when the low_priority tag is set on
    // sync
    let tags = BTreeMap::from([(TagName::LowPriority, TagInfo::default())]);
    mock_sync_with_tags(&server, &mut ev_builder, room_id, tags).await;
    sync_once(&client, &server).await;

    assert!(notable_tags_stream.next().await.unwrap().is_low_priority);
}
