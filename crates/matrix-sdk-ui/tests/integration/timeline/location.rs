use std::time::Duration;

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{assert_let_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_base::timeout::timeout;
use matrix_sdk_test::{BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use matrix_sdk_ui::timeline::{EventSendState, RoomExt};
use ruma::{
    event_id,
    events::{
        location::{AssetType, ZoomLevel},
        room::message::MessageType,
    },
    room_id,
};
use serde_json::json;
use stream_assert::assert_next_matches;
use tokio::task::yield_now;
use wiremock::{Request, ResponseTemplate};

#[async_test]
async fn test_send_location() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    server
        .mock_room_send()
        .respond_with(|req: &Request| {
            let content: serde_json::Value = req.body_json().unwrap();

            assert_eq!(content["msgtype"], "m.location");
            assert_eq!(content["body"], "Big Ben");
            assert_eq!(content["geo_uri"], "geo:51.5008,-0.1247");
            let location = &content["org.matrix.msc3488.location"];
            assert_eq!(location["uri"], "geo:51.5008,-0.1247");
            assert_eq!(location["description"], "Elizabeth Tower");
            assert_eq!(location["zoom_level"], 10);
            assert_eq!(content["org.matrix.msc3488.asset"]["type"], "m.pin");
            assert!(content.get("m.relates_to").is_none());

            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$location_event" }))
        })
        .expect(1)
        .mount()
        .await;

    timeline
        .send_location(
            "Big Ben".to_owned(),
            "geo:51.5008,-0.1247".to_owned(),
            Some("Elizabeth Tower".to_owned()),
            ZoomLevel::new(10),
            Some(AssetType::Pin),
            None,
        )
        .await
        .unwrap();

    yield_now().await;

    let item = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));
    let message = item.content().as_message().unwrap();
    assert_let!(MessageType::Location(location) = message.msgtype());
    assert_eq!(location.geo_uri, "geo:51.5008,-0.1247");

    let diff = timeout(timeline_stream.next(), Duration::from_secs(1)).await.unwrap().unwrap();
    assert_let!(VectorDiff::Set { index: 0, value: remote_echo } = diff);
    assert_matches!(remote_echo.send_state(), Some(EventSendState::Sent { .. }));
}

#[async_test]
async fn test_send_location_as_reply() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let event_id_from_bob = event_id!("$event_from_bob");
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("Where are you?").sender(&BOB).event_id(event_id_from_bob),
            ),
        )
        .await;

    assert_next_matches!(timeline_stream, VectorDiff::PushBack { .. });

    server
        .mock_room_send()
        .respond_with(move |req: &Request| {
            let content: serde_json::Value = req.body_json().unwrap();

            assert_eq!(content["msgtype"], "m.location");
            assert_eq!(
                content["m.relates_to"]["m.in_reply_to"]["event_id"],
                event_id_from_bob.as_str()
            );
            assert_eq!(content["m.mentions"]["user_ids"], json!([BOB.as_str()]));

            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$location_event" }))
        })
        .expect(1)
        .mount()
        .await;

    timeline
        .send_location(
            "Here".to_owned(),
            "geo:51.5008,-0.1247".to_owned(),
            None,
            None,
            None,
            Some(event_id_from_bob.to_owned()),
        )
        .await
        .unwrap();

    yield_now().await;

    let item = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    let msglike = item.content().as_msglike().unwrap();
    assert_eq!(msglike.in_reply_to.clone().unwrap().event_id, event_id_from_bob);
    let message = item.content().as_message().unwrap();
    assert_let!(MessageType::Location(location) = message.msgtype());
    assert_eq!(location.geo_uri, "geo:51.5008,-0.1247");

    let diff = timeout(timeline_stream.next(), Duration::from_secs(1)).await.unwrap().unwrap();
    assert_let!(VectorDiff::Set { index: 1, value: remote_echo } = diff);
    assert_matches!(remote_echo.send_state(), Some(EventSendState::Sent { .. }));
}

#[async_test]
async fn test_send_location_can_be_aborted() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    client.send_queue().set_enabled(false).await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let handle = timeline
        .send_location(
            "Big Ben".to_owned(),
            "geo:51.5008,-0.1247".to_owned(),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

    assert_let_timeout!(Some(VectorDiff::PushBack { value: item }) = timeline_stream.next());
    assert_matches!(item.send_state(), Some(EventSendState::NotSentYet { progress: None }));

    assert!(handle.abort().await.unwrap());

    assert_let_timeout!(Some(VectorDiff::Remove { index: 0 }) = timeline_stream.next());
}
