// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{pin::pin, time::Duration};

use assert_matches::assert_matches;
use async_trait::async_trait;
use futures_util::FutureExt;
use matrix_sdk::{
    test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    widget::{
        Capabilities, CapabilitiesProvider, WidgetDriver, WidgetDriverHandle, WidgetSettings,
    },
    Client,
};
use matrix_sdk_common::{executor::spawn, timeout::timeout};
use matrix_sdk_test::{async_test, event_factory::EventFactory, JoinedRoomBuilder, ALICE, BOB};
use once_cell::sync::Lazy;
use ruma::{
    event_id,
    events::{room::member::MembershipState, MessageLikeEventType, StateEventType},
    owned_room_id,
    serde::JsonObject,
    user_id, OwnedRoomId,
};
use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use tracing::error;
use wiremock::{
    matchers::{method, path_regex},
    Mock, ResponseTemplate,
};

/// Create a JSON string from a [`json!`][serde_json::json] "literal".
#[macro_export]
macro_rules! json_string {
    ($( $tt:tt )*) => { ::serde_json::json!( $($tt)* ).to_string() };
}

const WIDGET_ID: &str = "test-widget";
static ROOM_ID: Lazy<OwnedRoomId> = Lazy::new(|| owned_room_id!("!a98sd12bjh:example.org"));

async fn run_test_driver(
    init_on_content_load: bool,
) -> (Client, MatrixMockServer, WidgetDriverHandle) {
    struct DummyCapabilitiesProvider;

    #[async_trait]
    impl CapabilitiesProvider for DummyCapabilitiesProvider {
        async fn acquire_capabilities(&self, capabilities: Capabilities) -> Capabilities {
            // Grant all capabilities that the widget asks for
            capabilities
        }
    }
    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    let room = mock_server.sync_joined_room(&client, &ROOM_ID).await;
    mock_server.mock_room_state_encryption().plain().mount().await;

    let (driver, handle) = WidgetDriver::new(
        WidgetSettings::new(WIDGET_ID.to_owned(), init_on_content_load, "https://foo.bar/widget")
            .unwrap(),
    );

    spawn(async move {
        if let Err(()) = driver.run(room, DummyCapabilitiesProvider).await {
            error!("An error encountered in running the WidgetDriver (no details available yet)");
        }
    });

    (client, mock_server, handle)
}

async fn recv_message(driver_handle: &WidgetDriverHandle) -> JsonObject {
    let fut = pin!(driver_handle.recv());
    let msg = timeout(fut, Duration::from_secs(20)).await.unwrap();
    serde_json::from_str(&msg.unwrap()).unwrap()
}

async fn send_request(
    driver_handle: &WidgetDriverHandle,
    request_id: &str,
    action: &str,
    data: impl Serialize,
) {
    let json_string = json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": request_id,
        "action": action,
        "data": data,
    });
    println!("Json string sent from the widget {}", json_string);
    let sent = driver_handle.send(json_string).await;
    assert!(sent);
}

async fn send_response(
    driver_handle: &WidgetDriverHandle,
    request_id: &str,
    action: &str,
    request_data: impl Serialize,
    response_data: impl Serialize,
) {
    let sent = driver_handle
        .send(json_string!({
            "api": "toWidget",
            "widgetId": WIDGET_ID,
            "requestId": request_id,
            "action": action,
            "data": request_data,
            "response": response_data,
        }))
        .await;
    assert!(sent);
}

#[async_test]
async fn test_negotiate_capabilities_immediately() {
    let (_, _, driver_handle) = run_test_driver(false).await;

    let caps = json!(["org.matrix.msc2762.receive.event:m.room.message"]);

    {
        // Receive toWidget capabilities request
        let msg = recv_message(&driver_handle).await;
        assert_eq!(msg["api"], "toWidget");
        assert_eq!(msg["action"], "capabilities");
        let data = &msg["data"];
        let request_id = msg["requestId"].as_str().unwrap();

        // Let's send a request to get supported versions in the middle
        // of a capabilities negotiation to ensure that we're not "deadlocked" by
        // not processing messages while waiting for a reply from a widget to the
        // the toWidget request.
        {
            send_request(
                &driver_handle,
                "get-supported-api-versions",
                "supported_api_versions",
                json!({}),
            )
            .await;

            let msg = recv_message(&driver_handle).await;
            assert_eq!(msg["api"], "fromWidget");
            assert_eq!(msg["action"], "supported_api_versions");
            assert_eq!(msg["requestId"].as_str().unwrap(), "get-supported-api-versions");
            assert!(msg["response"]["supported_versions"].is_array());
        }

        // Answer with caps we want
        let response = json!({ "capabilities": caps });
        send_response(&driver_handle, request_id, "capabilities", data, &response).await;
    }

    {
        // Receive a "request" with the capabilities we were actually granted (wtf?)
        let msg = recv_message(&driver_handle).await;
        assert_eq!(msg["api"], "toWidget");
        assert_eq!(msg["action"], "notify_capabilities");
        assert_eq!(msg["data"], json!({ "requested": caps, "approved": caps }));
        let request_id = msg["requestId"].as_str().unwrap();

        // ACK the request
        send_response(&driver_handle, request_id, "notify_capabilities", caps, json!({})).await;
    }

    assert_matches!(driver_handle.recv().now_or_never(), None);
}

#[async_test]
async fn test_read_messages() {
    let (_, mock_server, driver_handle) = run_test_driver(true).await;

    {
        // Tell the driver that we're ready for communication
        send_request(&driver_handle, "1-content-loaded", "content_loaded", json!({})).await;

        // Receive the response
        let msg = recv_message(&driver_handle).await;
        assert_eq!(msg["api"], "fromWidget");
        assert_eq!(msg["action"], "content_loaded");
        assert!(msg["data"].as_object().unwrap().is_empty());
    }

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc2762.receive.event:m.room.message"]),
    )
    .await;

    // No messages from the driver
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);

    {
        let response_json = json!({
            "chunk": [
                {
                    "content": {
                        "body": "hello",
                        "msgtype": "m.text",
                    },
                    "event_id": "$msda7m0df9E9op3",
                    "origin_server_ts": 152037280,
                    "sender": "@example:localhost",
                    "type": "m.room.message",
                    "room_id": &*ROOM_ID,
                },
                {
                    "content": {
                        "body": "This room has been replaced",
                        "replacement_room": "!newroom:localhost",
                    },
                    "event_id": "$foun39djjod0f",
                    "origin_server_ts": 152039280,
                    "sender": "@bob:localhost",
                    "state_key": "",
                    "type": "m.room.tombstone",
                    "room_id": &*ROOM_ID,
                },
            ],
            "end": "t47409-4357353_219380_26003_2269",
            "start": "t392-516_47314_0_7_1_1_1_11444_1"
        });
        mock_server
            .mock_room_messages()
            .match_limit(2)
            .respond_with(ResponseTemplate::new(200).set_body_json(response_json))
            .mock_once()
            .mount()
            .await;

        // Ask the driver to read messages
        send_request(
            &driver_handle,
            "2-read-messages",
            "org.matrix.msc2876.read_events",
            json!({
                "type": "m.room.message",
                "limit": 2,
            }),
        )
        .await;

        // Receive the response
        let msg = recv_message(&driver_handle).await;
        assert_eq!(msg["api"], "fromWidget");
        assert_eq!(msg["action"], "org.matrix.msc2876.read_events");
        let events = msg["response"]["events"].as_array().unwrap();

        assert_eq!(events.len(), 1);
        let first_event = &events[0];
        assert_eq!(first_event["content"]["body"], "hello");
    }
}

#[async_test]
async fn test_read_messages_with_msgtype_capabilities() {
    let (_, mock_server, driver_handle) = run_test_driver(true).await;

    {
        // Tell the driver that we're ready for communication
        send_request(&driver_handle, "1-content-loaded", "content_loaded", json!({})).await;

        // Receive the response
        let msg = recv_message(&driver_handle).await;
        assert_eq!(msg["api"], "fromWidget");
        assert_eq!(msg["action"], "content_loaded");
        assert!(msg["data"].as_object().unwrap().is_empty());
    }

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc2762.receive.event:m.room.message#m.text"]),
    )
    .await;

    // No messages from the driver
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);

    let f = EventFactory::new().room(&ROOM_ID).sender(user_id!("@example:localhost"));

    {
        let end = "t47409-4357353_219380_26003_2269";
        let chunk2 = vec![
            f.notice("custom content").event_id(event_id!("$msda7m0df9E9op3")).into_raw_timeline(),
            f.text_msg("hello").event_id(event_id!("$msda7m0df9E9op5")).into_raw_timeline(),
            f.reaction(event_id!("$event_id"), "annotation").into_raw_timeline(),
        ];
        mock_server
            .mock_room_messages()
            .match_limit(3)
            .ok(RoomMessagesResponseTemplate::default().end_token(end).events(chunk2))
            .mock_once()
            .mount()
            .await;

        // Ask the driver to read messages
        send_request(
            &driver_handle,
            "2-read-messages",
            "org.matrix.msc2876.read_events",
            json!({
                "type": "m.room.message",
                "limit": 3,
            }),
        )
        .await;

        // Receive the response
        let msg = recv_message(&driver_handle).await;
        assert_eq!(msg["api"], "fromWidget");
        assert_eq!(msg["action"], "org.matrix.msc2876.read_events");
        let events = msg["response"]["events"].as_array().unwrap();

        assert_eq!(events.len(), 1);
        let first_event = &events[0];
        assert_eq!(first_event["content"]["body"], "hello");
    }
}

#[async_test]
async fn test_read_room_members() {
    let (_, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc2762.receive.state_event:m.room.member"]),
    )
    .await;

    // No messages from the driver
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);

    {
        // The read-events request is fulfilled from the state store
        drop(mock_server);

        // Ask the driver to read state events
        send_request(
            &driver_handle,
            "2-read-messages",
            "org.matrix.msc2876.read_events",
            json!({ "type": "m.room.member", "state_key": true }),
        )
        .await;

        // Receive the response
        let msg = recv_message(&driver_handle).await;
        assert_eq!(msg["api"], "fromWidget");
        assert_eq!(msg["action"], "org.matrix.msc2876.read_events");
        let events = msg["response"]["events"].as_array().unwrap();

        // No useful data in the state store, that's fine for this test
        // (we just want to know that a successful response is generated)
        assert_eq!(events.len(), 0);
    }
}

#[async_test]
async fn test_receive_live_events() {
    let (client, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!([
            "org.matrix.msc2762.receive.event:m.room.member",
            "org.matrix.msc2762.receive.event:m.room.message#m.text",
            "org.matrix.msc2762.receive.state_event:m.room.name#",
            "org.matrix.msc2762.receive.state_event:m.room.member#@example:localhost",
            "org.matrix.msc2762.receive.state_event:m.room.member#@example:localhost",
            "org.matrix.msc3819.receive.to_device:my.custom.to.device"
        ]),
    )
    .await;

    // No messages from the driver yet
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);

    let f = EventFactory::new();

    mock_server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_joined_room(
                JoinedRoomBuilder::new(&ROOM_ID)
                    // text message from alice - matches filter #2
                    .add_timeline_event(f.text_msg("simple text message").sender(&ALICE))
                    // emote from alice - doesn't match
                    .add_timeline_event(f.emote("emote message").sender(&ALICE))
                    // pointless member event - matches filter #4
                    .add_timeline_event(
                        f.member(user_id!("@example:localhost"))
                            .membership(MembershipState::Join)
                            .previous(MembershipState::Join),
                    )
                    // kick alice - doesn't match because the `#@example:localhost` bit
                    // is about the state_key, not the sender
                    .add_timeline_event(
                        f.member(user_id!("@example:localhost"))
                            .banned(&ALICE)
                            .previous(MembershipState::Join),
                    )
                    // set room topic - doesn't match
                    .add_timeline_event(f.room_topic("new room topic").sender(&BOB))
                    // set room name - matches filter #3
                    .add_timeline_event(f.room_name("New Room Name").sender(&BOB)),
            );
            // to device message - doesn't match
            sync_builder.add_to_device_event(json!({
              "sender": "@alice:example.com",
              "type": "m.not_matching.to.device",
              "content": {
                "a": "test",
              }
            }));
            // to device message - matches filter #6
            sync_builder.add_to_device_event(json!({
                    "sender": "@alice:example.com",
                    "type": "my.custom.to.device",
                    "content": {
                      "a": "test",
                    }
                  }
            ));
        })
        .await;

    // The to device and room events are racing -> we dont know the order and just
    // need to store them separately.
    let mut to_device: JsonObject = JsonObject::new();
    let mut events = vec![];
    for _ in 0..4 {
        let msg = recv_message(&driver_handle).await;
        if msg["action"] == "send_to_device" {
            to_device = msg;
        } else {
            events.push(msg);
        }
    }

    assert_eq!(events[0]["api"], "toWidget");
    assert_eq!(events[0]["action"], "send_event");
    assert_eq!(events[0]["data"]["type"], "m.room.message");
    assert_eq!(events[0]["data"]["room_id"], ROOM_ID.as_str());
    assert_eq!(events[0]["data"]["content"]["msgtype"], "m.text");
    assert_eq!(events[0]["data"]["content"]["body"], "simple text message");

    assert_eq!(events[1]["api"], "toWidget");
    assert_eq!(events[1]["action"], "send_event");
    assert_eq!(events[1]["data"]["type"], "m.room.member");
    assert_eq!(events[1]["data"]["room_id"], ROOM_ID.as_str());
    assert_eq!(events[1]["data"]["state_key"], "@example:localhost");
    assert_eq!(events[1]["data"]["content"]["membership"], "join");
    assert_eq!(events[1]["data"]["unsigned"]["prev_content"]["membership"], "join");

    assert_eq!(events[2]["api"], "toWidget");
    assert_eq!(events[2]["action"], "send_event");
    assert_eq!(events[2]["data"]["type"], "m.room.name");
    assert_eq!(events[2]["data"]["sender"], BOB.as_str());
    assert_eq!(events[2]["data"]["content"]["name"], "New Room Name");

    assert_eq!(to_device["api"], "toWidget");
    assert_eq!(to_device["action"], "send_to_device");
    assert_eq!(to_device["data"]["type"], "my.custom.to.device");

    // No more messages from the driver
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);
}

#[async_test]
async fn test_send_room_message() {
    let (_, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(&driver_handle, json!(["org.matrix.msc2762.send.event:m.room.message"]))
        .await;

    mock_server
        .mock_room_send()
        .for_type("m.room.message".into())
        .ok(event_id!("$foobar"))
        .mock_once()
        .mount()
        .await;

    send_request(
        &driver_handle,
        "send-room-message",
        "send_event",
        json!({
            "type": "m.room.message",
            "content": {
                "msgtype": "m.text",
                "body": "Message from a widget!",
            },
        }),
    )
    .await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_event");
    let event_id = msg["response"]["event_id"].as_str().unwrap();
    assert_eq!(event_id, "$foobar");
}

#[async_test]
async fn test_send_room_name() {
    let (_, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc2762.send.state_event:m.room.name#"]),
    )
    .await;

    mock_server
        .mock_room_send_state()
        .for_type(StateEventType::RoomName)
        .ok(event_id!("$foobar"))
        .mock_once()
        .mount()
        .await;

    send_request(
        &driver_handle,
        "send-room-name",
        "send_event",
        json!({
            "type": "m.room.name",
            "state_key": "",
            "content": {
                "name": "Room Name set by Widget",
            },
        }),
    )
    .await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_event");
    let event_id = msg["response"]["event_id"].as_str().unwrap();
    assert_eq!(event_id, "$foobar");
}

#[async_test]
async fn test_send_delayed_message_event() {
    let (_, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!([
            "org.matrix.msc4157.send.delayed_event",
            "org.matrix.msc2762.send.event:m.room.message"
        ]),
    )
    .await;
    mock_server
        .mock_room_send()
        .match_delayed_event(Duration::from_millis(1000))
        .for_type(MessageLikeEventType::RoomMessage)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "delay_id": "1234",
        })))
        .mock_once()
        .mount()
        .await;

    send_request(
        &driver_handle,
        "send-room-message",
        "send_event",
        json!({
            "type": "m.room.message",
            "content": {
                "msgtype": "m.text",
                "body": "Message from a widget!",
            },
            "delay":1000,
        }),
    )
    .await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_event");
    let delay_id = msg["response"]["delay_id"].as_str().unwrap();
    assert_eq!(delay_id, "1234");
}

#[async_test]
async fn test_send_delayed_state_event() {
    let (_, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!([
            "org.matrix.msc4157.send.delayed_event",
            "org.matrix.msc2762.send.state_event:m.room.name#"
        ]),
    )
    .await;

    mock_server
        .mock_room_send_state()
        .match_delayed_event(Duration::from_millis(1000))
        .for_type(StateEventType::RoomName)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "delay_id": "1234",
        })))
        .mock_once()
        .mount()
        .await;

    send_request(
        &driver_handle,
        "send-room-message",
        "send_event",
        json!({
            "type": "m.room.name",
            "state_key": "",
            "content": {
                "name": "Room Name set by Widget",
            },
            "delay":1000,
        }),
    )
    .await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_event");
    let delay_id = msg["response"]["delay_id"].as_str().unwrap();
    assert_eq!(delay_id, "1234");
}

#[async_test]
async fn test_fail_sending_delay_rate_limit() {
    let (_, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!([
            "org.matrix.msc4157.send.delayed_event",
            "org.matrix.msc2762.send.event:m.room.message"
        ]),
    )
    .await;

    mock_server
        .mock_room_send()
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "errcode": "M_LIMIT_EXCEEDED",
            "error": "Sending too many delay events"
        })))
        .mock_once()
        .mount()
        .await;

    send_request(
        &driver_handle,
        "send-room-message",
        "send_event",
        json!({
            "type": "m.room.message",
            "content": {
                "msgtype": "m.text",
                "body": "Message from a widget!",
            },
            "delay":1000,
        }),
    )
    .await;

    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_event");
    // Receive the response in the correct widget error response format
    assert_eq!(
        msg["response"],
        json!({
            "error": {
              "matrix_api_error": {
                "http_status": 400,
                "response": {
                  "errcode": "M_LIMIT_EXCEEDED",
                  "error": "Sending too many delay events"
                },
              },
              "message": "the server returned an error: [400 / M_LIMIT_EXCEEDED] Sending too many delay events"
            }
        })
    );
}

#[async_test]
async fn test_try_send_delayed_state_event_without_permission() {
    let (_, _mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc2762.send.state_event:m.room.name#"]),
    )
    .await;

    send_request(
        &driver_handle,
        "send-room-message",
        "send_event",
        json!({
            "type": "m.room.name",
            "state_key": "",
            "content": {
                "name": "Room Name set by Widget",
            },
            "delay":1000,
        }),
    )
    .await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_event");
    let error_message = msg["response"]["error"]["message"].as_str().unwrap();
    assert_eq!(
        error_message,
        "Not allowed: missing the org.matrix.msc4157.send.delayed_event capability."
    );
}

#[async_test]
async fn test_update_delayed_event() {
    let (_, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(&driver_handle, json!(["org.matrix.msc4157.update_delayed_event",]))
        .await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/unstable/org.matrix.msc4140/delayed_events/1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(mock_server.server())
        .await;

    send_request(
        &driver_handle,
        "send-delay-update-id",
        "org.matrix.msc4157.update_delayed_event",
        json!({
            "action":"refresh",
            "delay_id": "1234",
        }),
    )
    .await;
    // Receive the response
    let response = recv_message(&driver_handle).await;
    print!("{:?}", response);
    assert_eq!(response["api"], "fromWidget");
    assert_eq!(response["action"], "org.matrix.msc4157.update_delayed_event");
    let empty_response = response["response"].clone();
    assert_eq!(empty_response, serde_json::from_str::<JsonValue>("{}").unwrap());
}

#[async_test]
async fn test_try_update_delayed_event_without_permission() {
    let (_, _mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(&driver_handle, json!([])).await;

    send_request(
        &driver_handle,
        "send-delay-update-id",
        "org.matrix.msc4157.update_delayed_event",
        json!({
            "action":"refresh",
            "delay_id": "1234",
        }),
    )
    .await;
    // Receive the response
    let response = recv_message(&driver_handle).await;
    print!("{:?}", response);
    assert_eq!(response["api"], "fromWidget");
    assert_eq!(response["action"], "org.matrix.msc4157.update_delayed_event");
    let error_response = response["response"]["error"]["message"].clone();
    assert_eq!(
        error_response.as_str().unwrap(),
        "Not allowed: missing the org.matrix.msc4157.update_delayed_event capability."
    );
}

#[async_test]
async fn test_try_update_delayed_event_without_permission_negotiate() {
    let (_, _mock_server, driver_handle) = run_test_driver(false).await;

    send_request(
        &driver_handle,
        "send-delay-update-id",
        "org.matrix.msc4157.update_delayed_event",
        json!({
            "action":"refresh",
            "delay_id": "1234",
        }),
    )
    .await;
    // Wait for the corresponding response.
    loop {
        let response = recv_message(&driver_handle).await;
        if response["api"] == "fromWidget"
            && response["action"] == "org.matrix.msc4157.update_delayed_event"
        {
            let error_response = response["response"]["error"]["message"].clone();
            assert_eq!(
                error_response.as_str().unwrap(),
                "Received send update delayed event request before capabilities were negotiated"
            );
            break;
        }
    }
}

#[async_test]
async fn test_send_redaction() {
    let (_, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!([
            // "org.matrix.msc4157.send.delayed_event",
            "org.matrix.msc2762.send.event:m.room.redaction"
        ]),
    )
    .await;
    mock_server.mock_room_redact().ok(event_id!("$redact_event_id")).mock_once().mount().await;

    send_request(
        &driver_handle,
        "send-redact-message",
        "send_event",
        json!({
            "type": "m.room.redaction",
            "content": {
                "redacts": "$1234"
            },
        }),
    )
    .await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_event");
    let redact_event_id = msg["response"]["event_id"].as_str().unwrap();
    let redact_room_id = msg["response"]["room_id"].as_str().unwrap();

    assert_eq!(redact_event_id, "$redact_event_id");
    assert_eq!(redact_room_id, "!a98sd12bjh:example.org");
}

async fn send_to_device_test_helper(
    request_id: &str,
    data: JsonValue,
    expected_response: JsonValue,
    calls: u64,
) -> JsonValue {
    let (_, mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!([
            "org.matrix.msc3819.send.to_device:my.custom.to_device_type",
            "org.matrix.msc3819.send.to_device:my.other_type"
        ]),
    )
    .await;

    mock_server.mock_send_to_device().ok().expect(calls).mount().await;

    send_request(&driver_handle, request_id, "send_to_device", data).await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_to_device");
    let response = msg["response"].clone();
    assert_eq!(
        serde_json::to_string(&response).unwrap(),
        serde_json::to_string(&expected_response).unwrap()
    );

    response
}

#[async_test]
async fn test_send_to_device_event() {
    send_to_device_test_helper(
        "id_my.custom.to_device_type",
        json!({
            "type":"my.custom.to_device_type",
            "encrypted": false,
            "messages":{
                "@username:test.org": {
                    "DEVICEID": {
                        "param1":"test",
                    },
                },
            }
        }),
        json! {{}},
        1,
    )
    .await;
}

#[async_test]
async fn test_error_to_device_event_no_permission() {
    send_to_device_test_helper(
        "id_my.unallowed_type",
        json!({
            "type": "my.unallowed_type",
            "encrypted": false,
            "messages": {
                "@username:test.org": {
                    "DEVICEID": {
                        "param1":"test",
                    },
                },
            }
        }),
        // this means the server did not get the correct event type
        json! {{"error": {"message": "Not allowed to send to-device message of type: my.unallowed_type"}}},
        0
    )
    .await;
}

#[async_test]
async fn test_send_encrypted_to_device_event() {
    send_to_device_test_helper(
        "my.custom.to_device_type",
        json!({
            "type": "my.custom.to_device_type",
            "encrypted": true,
            "messages":{
                "@username:test.org": {
                    "DEVICEID": {
                        "param1":"test",
                    },
                },
            }
        }),
        json! {{}},
        1,
    )
    .await;
}

async fn negotiate_capabilities(driver_handle: &WidgetDriverHandle, caps: JsonValue) {
    {
        // Receive toWidget capabilities request
        let msg = recv_message(driver_handle).await;
        assert_eq!(msg["api"], "toWidget");
        assert_eq!(msg["action"], "capabilities");
        let data = &msg["data"];
        let request_id = msg["requestId"].as_str().unwrap();

        // Answer with caps we want
        let response = json!({ "capabilities": caps });
        send_response(driver_handle, request_id, "capabilities", data, &response).await;
    }

    {
        // Receive notification with granted capabilities
        let msg = recv_message(driver_handle).await;
        assert_eq!(msg["api"], "toWidget");
        assert_eq!(msg["action"], "notify_capabilities");
        assert_eq!(msg["data"], json!({ "requested": caps, "approved": caps }));
        let request_id = msg["requestId"].as_str().unwrap();

        // ACK the notification
        send_response(driver_handle, request_id, "notify_capabilities", caps, json!({})).await;
    }
}
