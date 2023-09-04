use std::time::Duration;

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::config::SyncSettings;
use matrix_sdk_test::{async_test, JoinedRoomBuilder, SyncResponseBuilder, TimelineTestEvent};
use matrix_sdk_ui::timeline::{
    Error as TimelineError, RoomExt, TimelineDetails, TimelineItemContent,
};
use ruma::{event_id, room_id};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync};

#[async_test]
async fn in_reply_to_details() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let (_, mut timeline_stream) = timeline.subscribe().await;

    // The event doesn't exist.
    assert_matches!(
        timeline.fetch_details_for_event(event_id!("$fakeevent")).await,
        Err(TimelineError::RemoteEventNotInTimeline)
    );

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": "$event1",
                "origin_server_ts": 152037280,
                "sender": "@alice:example.org",
                "type": "m.room.message",
            })))
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {
                    "body": "hello to you too",
                    "msgtype": "m.text",
                    "m.relates_to": {
                        "m.in_reply_to": {
                            "event_id": "$event1",
                        },
                    },
                },
                "event_id": "$event2",
                "origin_server_ts": 152045456,
                "sender": "@bob:example.org",
                "type": "m.room.message",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let _day_divider = assert_matches!(timeline_stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let first = assert_matches!(timeline_stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    assert_matches!(first.as_event().unwrap().content(), TimelineItemContent::Message(_));
    let second = assert_matches!(timeline_stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let second_event = second.as_event().unwrap();
    let message =
        assert_matches!(second_event.content(), TimelineItemContent::Message(message) => message);
    let in_reply_to = message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id!("$event1"));
    assert_matches!(in_reply_to.event, TimelineDetails::Ready(_));

    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "body": "you were right",
                "msgtype": "m.text",
                "m.relates_to": {
                    "m.in_reply_to": {
                        "event_id": "$remoteevent",
                    },
                },
            },
            "event_id": "$event3",
            "origin_server_ts": 152046694,
            "sender": "@bob:example.org",
            "type": "m.room.message",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let _read_receipt_update =
        assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { value, .. }) => value);

    let third = assert_matches!(
        timeline_stream.next().await,
        Some(VectorDiff::PushBack { value }) => value
    );
    let third_event = third.as_event().unwrap();
    let message =
        assert_matches!(third_event.content(), TimelineItemContent::Message(message) => message);
    let in_reply_to = message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id!("$remoteevent"));
    assert_matches!(in_reply_to.event, TimelineDetails::Unavailable);

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/\$remoteevent"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Event not found.",
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Fetch details remotely if we can't find them locally.
    timeline.fetch_details_for_event(third_event.event_id().unwrap()).await.unwrap();
    server.reset().await;

    let third = assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 3, value }) => value);
    let message = assert_matches!(third.as_event().unwrap().content(), TimelineItemContent::Message(message) => message);
    assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Pending);

    let third = assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 3, value }) => value);
    let message = assert_matches!(third.as_event().unwrap().content(), TimelineItemContent::Message(message) => message);
    assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Error(_));

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/\$remoteevent"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": {
                "body": "Alice is gonna arrive soon",
                "msgtype": "m.text",
            },
            "room_id": room_id,
            "event_id": "$event0",
            "origin_server_ts": 152024004,
            "sender": "@admin:example.org",
            "type": "m.room.message",
        })))
        .expect(1)
        .mount(&server)
        .await;

    timeline.fetch_details_for_event(third_event.event_id().unwrap()).await.unwrap();

    let third = assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 3, value }) => value);
    let message = assert_matches!(third.as_event().unwrap().content(), TimelineItemContent::Message(message) => message);
    assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Pending);

    let third = assert_matches!(timeline_stream.next().await, Some(VectorDiff::Set { index: 3, value }) => value);
    let message = assert_matches!(third.as_event().unwrap().content(), TimelineItemContent::Message(message) => message);
    assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Ready(_));
}
