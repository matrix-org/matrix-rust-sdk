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

use std::time::Duration;

use assert_matches::assert_matches;
use async_trait::async_trait;
use futures_util::FutureExt;
use matrix_sdk::{
    config::SyncSettings,
    widget::{
        Capabilities, CapabilitiesProvider, WidgetDriver, WidgetDriverHandle, WidgetSettings,
    },
};
use matrix_sdk_common::executor::spawn;
use matrix_sdk_test::{async_test, JoinedRoomBuilder, SyncResponseBuilder};
use once_cell::sync::Lazy;
use ruma::{owned_room_id, serde::JsonObject, OwnedRoomId};
use serde::Serialize;
use serde_json::{json, Value as JsonValue};
use tracing::error;
use wiremock::{
    matchers::{header, method, path_regex, query_param},
    Mock, MockServer, ResponseTemplate,
};

use crate::{logged_in_client, mock_encryption_state, mock_sync};

/// Create a JSON string from a [`json!`][serde_json::json] "literal".
#[macro_export]
macro_rules! json_string {
    ($( $tt:tt )*) => { ::serde_json::json!( $($tt)* ).to_string() };
}

const WIDGET_ID: &str = "test-widget";
static ROOM_ID: Lazy<OwnedRoomId> = Lazy::new(|| owned_room_id!("!a98sd12bjh:example.org"));

async fn run_test_driver(init_on_content_load: bool) -> (MockServer, WidgetDriverHandle) {
    struct DummyPermissionsProvider;

    #[async_trait]
    impl CapabilitiesProvider for DummyPermissionsProvider {
        async fn acquire_capabilities(&self, capabilities: Capabilities) -> Capabilities {
            // Grant all capabilities that the widget asks for
            capabilities
        }
    }

    let (client, mock_server) = logged_in_client().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(ROOM_ID.clone()));

    mock_sync(&mock_server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    mock_server.reset().await;

    mock_encryption_state(&mock_server, false).await;

    let room = client.get_room(&ROOM_ID).unwrap();

    let (driver, handle) = WidgetDriver::new(
        WidgetSettings::new(WIDGET_ID.to_owned(), init_on_content_load, "https://foo.bar/widget")
            .unwrap(),
    );

    spawn(async move {
        if let Err(()) = driver.run(room, DummyPermissionsProvider).await {
            error!("An error encountered in running the WidgetDriver (no details available yet)");
        }
    });

    (mock_server, handle)
}

async fn recv_message(driver_handle: &WidgetDriverHandle) -> JsonObject {
    serde_json::from_str(&driver_handle.recv().await.unwrap()).unwrap()
}

async fn send_request(
    driver_handle: &WidgetDriverHandle,
    request_id: &str,
    action: &str,
    data: impl Serialize,
) {
    let sent = driver_handle
        .send(json_string!({
            "api": "fromWidget",
            "widgetId": WIDGET_ID,
            "requestId": request_id,
            "action": action,
            "data": data,
        }))
        .await;
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
async fn negotiate_capabilities_immediately() {
    let (_, driver_handle) = run_test_driver(false).await;

    let caps = json!(["org.matrix.msc2762.receive.event:m.room.message"]);

    {
        // Receive toWidget capabilities request
        let msg = recv_message(&driver_handle).await;
        assert_eq!(msg["api"], "toWidget");
        assert_eq!(msg["action"], "capabilities");
        let data = &msg["data"];
        let request_id = msg["requestId"].as_str().unwrap();

        // Answer with caps we want
        send_response(&driver_handle, request_id, "capabilities", data, &caps).await;
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
#[allow(unused)] // test is incomplete
async fn read_messages() {
    let (mock_server, driver_handle) = run_test_driver(true).await;

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
    return; // TODO: Test ends here for now

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
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
            .and(header("authorization", "Bearer 1234"))
            .and(query_param("limit", "2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response_json))
            .expect(1)
            .mount(&mock_server)
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

        assert_eq!(events.len(), 2);
        let first_event = &events[0];
        assert_eq!(first_event["content"]["body"], "hello");
    }

    mock_server.verify().await;
}

#[async_test]
async fn read_room_members() {
    let (mock_server, driver_handle) = run_test_driver(false).await;

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
async fn send_room_message() {
    let (mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(&driver_handle, json!(["org.matrix.msc2762.send.event:m.room.message"]))
        .await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/m.room.message/.*$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$foobar" })))
        .expect(1)
        .mount(&mock_server)
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

    // Make sure the event-sending endpoint was hit exactly once
    mock_server.verify().await;
}

#[async_test]
async fn send_room_name() {
    let (mock_server, driver_handle) = run_test_driver(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc2762.send.state_event:m.room.name#"]),
    )
    .await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.name/?$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$foobar" })))
        .expect(1)
        .mount(&mock_server)
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

    // Make sure the event-sending endpoint was hit exactly once
    mock_server.verify().await;
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
        send_response(driver_handle, request_id, "capabilities", data, &caps).await;
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
