#![cfg(feature = "experimental-timeline")]

use std::{sync::Arc, time::Duration};

use assert_matches::assert_matches;
use futures_signals::signal_vec::{SignalVecExt, VecDiff};
use futures_util::StreamExt;
use matrix_sdk::{
    config::SyncSettings,
    room::timeline::{TimelineDetails, TimelineItemContent, TimelineKey, VirtualTimelineItem},
    ruma::MilliSecondsSinceUnixEpoch,
};
use matrix_sdk_common::executor::spawn;
use matrix_sdk_test::{
    async_test, test_json, EventBuilder, JoinedRoomBuilder, RoomAccountDataTestEvent,
    TimelineTestEvent,
};
use ruma::{
    event_id,
    events::room::message::{MessageType, RoomMessageEventContent},
    room_id, uint, user_id, TransactionId,
};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client, mock_sync};

#[async_test]
async fn edit() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let mut timeline_stream = timeline.signal().to_stream();

    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "body": "hello",
                "msgtype": "m.text",
            },
            "event_id": "$msda7m:localhost",
            "origin_server_ts": 152037280,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let first =
        assert_matches!(timeline_stream.next().await, Some(VecDiff::Push { value }) => value);
    let msg = assert_matches!(
        first.as_event().unwrap().content(),
        TimelineItemContent::Message(msg) => msg
    );
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert_matches!(msg.in_reply_to(), None);
    assert!(!msg.is_edited());

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {
                    "body": "Test",
                    "formatted_body": "<em>Test</em>",
                    "msgtype": "m.text",
                    "format": "org.matrix.custom.html",
                },
                "event_id": "$7at8sd:localhost",
                "origin_server_ts": 152038280,
                "sender": "@bob:example.org",
                "type": "m.room.message",
            })))
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {
                    "body": " * hi",
                    "m.new_content": {
                        "body": "hi",
                        "msgtype": "m.text",
                    },
                    "m.relates_to": {
                        "event_id": "$msda7m:localhost",
                        "rel_type": "m.replace",
                    },
                    "msgtype": "m.text",
                },
                "event_id": "$msda7m2:localhost",
                "origin_server_ts": 159056300,
                "sender": "@alice:example.org",
                "type": "m.room.message",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let second =
        assert_matches!(timeline_stream.next().await, Some(VecDiff::Push { value }) => value);
    let item = second.as_event().unwrap();
    assert_eq!(item.origin_server_ts(), Some(MilliSecondsSinceUnixEpoch(uint!(152038280))));
    assert!(item.event_id().is_some());
    assert!(!item.is_own());
    assert!(item.raw().is_some());

    let msg = assert_matches!(item.content(), TimelineItemContent::Message(msg) => msg);
    assert_matches!(msg.msgtype(), MessageType::Text(_));
    assert_matches!(msg.in_reply_to(), None);
    assert!(!msg.is_edited());

    let edit = assert_matches!(
        timeline_stream.next().await,
        Some(VecDiff::UpdateAt { index: 0, value }) => value
    );
    let edited = assert_matches!(
        edit.as_event().unwrap().content(),
        TimelineItemContent::Message(msg) => msg
    );
    let text = assert_matches!(edited.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "hi");
    assert_matches!(edited.in_reply_to(), None);
    assert!(edited.is_edited());
}

#[async_test]
async fn echo() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let mut timeline_stream = timeline.signal().to_stream();

    let event_id = event_id!("$wWgymRfo7ri1uQx0NXO40vLJ");
    let txn_id: &TransactionId = "my-txn-id".into();

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&json!({ "event_id": event_id })))
        .mount(&server)
        .await;

    // Don't move the original timeline, it must live until the end of the test
    let timeline = timeline.clone();
    let send_hdl = spawn(async move {
        timeline
            .send(RoomMessageEventContent::text_plain("Hello, World!").into(), Some(txn_id))
            .await
    });

    let local_echo =
        assert_matches!(timeline_stream.next().await, Some(VecDiff::Push { value }) => value);
    let item = local_echo.as_event().unwrap();
    assert!(item.event_id().is_none());
    assert!(item.is_own());
    assert_matches!(item.key(), TimelineKey::TransactionId(_));
    assert_eq!(item.origin_server_ts(), None);
    assert_matches!(item.raw(), None);

    let msg = assert_matches!(item.content(), TimelineItemContent::Message(msg) => msg);
    let text = assert_matches!(msg.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "Hello, World!");

    // Wait for the sending to finish and assert everything was successful
    send_hdl.await.unwrap().unwrap();

    let sent_confirmation = assert_matches!(
        timeline_stream.next().await,
        Some(VecDiff::UpdateAt { index: 0, value }) => value
    );
    let item = sent_confirmation.as_event().unwrap();
    assert!(item.event_id().is_some());
    assert!(item.is_own());
    assert_matches!(item.key(), TimelineKey::TransactionId(_));
    assert_eq!(item.origin_server_ts(), None);
    assert_matches!(item.raw(), None);

    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "body": "Hello, World!",
                "msgtype": "m.text",
            },
            "event_id": "$7at8sd:localhost",
            "origin_server_ts": 152038280,
            "sender": "@example:localhost",
            "type": "m.room.message",
            "unsigned": { "transaction_id": txn_id, },
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let remote_echo = assert_matches!(
        timeline_stream.next().await,
        Some(VecDiff::UpdateAt { index: 0, value }) => value
    );
    let item = remote_echo.as_event().unwrap();
    assert!(item.event_id().is_some());
    assert!(item.is_own());
    assert_eq!(item.origin_server_ts(), Some(MilliSecondsSinceUnixEpoch(uint!(152038280))));
    assert_matches!(item.key(), TimelineKey::EventId(_));
    assert_matches!(item.raw(), Some(_));
}

#[async_test]
async fn back_pagination() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = Arc::new(room.timeline().await);
    let mut timeline_stream = timeline.signal().to_stream();

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::ROOM_MESSAGES_BATCH_1))
        .expect(1)
        .named("messages_batch_1")
        .mount(&server)
        .await;

    timeline.paginate_backwards(uint!(10)).await.unwrap();

    let message = assert_matches!(
        timeline_stream.next().await,
        Some(VecDiff::Push { value }) => value
    );
    let msg = assert_matches!(
        message.as_event().unwrap().content(),
        TimelineItemContent::Message(msg) => msg
    );
    let text = assert_matches!(msg.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "hello world");

    let message = assert_matches!(
        timeline_stream.next().await,
        Some(VecDiff::InsertAt { index: 0, value }) => value
    );
    let msg = assert_matches!(
        message.as_event().unwrap().content(),
        TimelineItemContent::Message(msg) => msg
    );
    let text = assert_matches!(msg.msgtype(), MessageType::Text(text) => text);
    assert_eq!(text.body, "the world is big");
}

#[async_test]
async fn reaction() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let mut timeline_stream = timeline.signal().to_stream();

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": "$TTvQUp1e17qkw41rBSjpZ",
                "origin_server_ts": 152037280,
                "sender": "@alice:example.org",
                "type": "m.room.message",
            })))
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {
                    "m.relates_to": {
                        "event_id": "$TTvQUp1e17qkw41rBSjpZ",
                        "key": "👍",
                        "rel_type": "m.annotation",
                    },
                },
                "event_id": "$031IXQRi27504",
                "origin_server_ts": 152038300,
                "sender": "@bob:example.org",
                "type": "m.reaction",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let message =
        assert_matches!(timeline_stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(message.as_event().unwrap().content(), TimelineItemContent::Message(_));

    let updated_message = assert_matches!(
        timeline_stream.next().await,
        Some(VecDiff::UpdateAt { index: 0, value }) => value
    );
    let event_item = updated_message.as_event().unwrap();
    let msg = assert_matches!(event_item.content(), TimelineItemContent::Message(msg) => msg);
    assert!(!msg.is_edited());
    assert_eq!(event_item.reactions().len(), 1);
    let details = &event_item.reactions()["👍"];
    assert_eq!(details.count, uint!(1));
    let senders = assert_matches!(&details.senders, TimelineDetails::Ready(s) => s);
    assert_eq!(*senders, vec![user_id!("@bob:example.org").to_owned()]);

    // TODO: After adding raw timeline items, check for one here

    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {},
            "redacts": "$031IXQRi27504",
            "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
            "sender": "@bob:example.org",
            "origin_server_ts": 152037280,
            "type": "m.room.redaction",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let updated_message = assert_matches!(
        timeline_stream.next().await,
        Some(VecDiff::UpdateAt { index: 0, value }) => value
    );
    let event_item = updated_message.as_event().unwrap();
    let msg = assert_matches!(event_item.content(), TimelineItemContent::Message(msg) => msg);
    assert!(!msg.is_edited());
    assert_eq!(event_item.reactions().len(), 0);
}

#[async_test]
async fn redacted_message() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let mut timeline_stream = timeline.signal().to_stream();

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {},
                "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
                "origin_server_ts": 152035910,
                "sender": "@alice:example.org",
                "type": "m.room.message",
                "unsigned": {
                    "redacted_because": {
                        "content": {},
                        "redacts": "$eeG0HA0FAZ37wP8kXlNkxx3I",
                        "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
                        "sender": "@alice:example.org",
                        "origin_server_ts": 152037280,
                        "type": "m.room.redaction",
                    },
                },
            })))
            .add_timeline_event(TimelineTestEvent::Custom(json!({
                "content": {},
                "redacts": "$eeG0HA0FAZ37wP8kXlNkxx3I",
                "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
                "sender": "@alice:example.org",
                "origin_server_ts": 152037280,
                "type": "m.room.redaction",
            }))),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let first =
        assert_matches!(timeline_stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(first.as_event().unwrap().content(), TimelineItemContent::RedactedMessage);

    // TODO: After adding raw timeline items, check for one here
}

#[async_test]
async fn read_marker() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = EventBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await;
    let mut timeline_stream = timeline.signal().to_stream();

    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        TimelineTestEvent::Custom(json!({
            "content": {
                "body": "hello",
                "msgtype": "m.text",
            },
            "event_id": "$someplace:example.org",
            "origin_server_ts": 152037280,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        })),
    ));

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let message =
        assert_matches!(timeline_stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(message.as_event().unwrap().content(), TimelineItemContent::Message(_));

    ev_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_account_data(RoomAccountDataTestEvent::FullyRead),
    );

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let marker =
        assert_matches!(timeline_stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(marker.as_virtual().unwrap(), VirtualTimelineItem::ReadMarker);
}
