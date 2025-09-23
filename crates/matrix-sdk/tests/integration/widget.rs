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

use std::{future, pin::pin, sync::Arc, time::Duration};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use futures_util::FutureExt;
use matrix_sdk::{
    Client,
    test_utils::mocks::{
        MatrixMockServer, RoomMessagesResponseTemplate, encryption::PendingToDeviceMessages,
    },
    widget::{
        Capabilities, CapabilitiesProvider, WidgetDriver, WidgetDriverHandle, WidgetSettings,
    },
};
use matrix_sdk_base::crypto::CollectStrategy;
use matrix_sdk_common::{
    deserialized_responses::EncryptionInfo, executor::spawn, locks::Mutex, timeout::timeout,
};
use matrix_sdk_test::{
    ALICE, BOB, JoinedRoomBuilder, StateTestEvent, async_test, event_factory::EventFactory,
};
use once_cell::sync::Lazy;
use ruma::{
    OwnedRoomId,
    api::client::to_device::send_event_to_device::v3::Messages,
    device_id, event_id,
    events::{
        AnySyncStateEvent, AnyToDeviceEvent, MessageLikeEventType, StateEventType,
        room::{member::MembershipState, message::RoomMessageEventContent},
    },
    owned_room_id, room_id,
    serde::{JsonObject, Raw},
    to_device::DeviceIdOrAllDevices,
    user_id,
};
use serde::Serialize;
use serde_json::{Value as JsonValue, Value, json};
use tracing::error;
use wiremock::{
    Mock, Request, ResponseTemplate,
    matchers::{method, path_regex},
};

/// Create a JSON string from a [`json!`][serde_json::json] "literal".
#[macro_export]
macro_rules! json_string {
    ($( $tt:tt )*) => { ::serde_json::json!( $($tt)* ).to_string() };
}

/// A helper type for test that needs to capture to-device messages via event
/// handlers.
type HandledDeviceEventMutex = Arc<Mutex<(Option<Raw<AnyToDeviceEvent>>, Option<EncryptionInfo>)>>;

const WIDGET_ID: &str = "test-widget";
static ROOM_ID: Lazy<OwnedRoomId> = Lazy::new(|| owned_room_id!("!a98sd12bjh:example.org"));

struct DummyCapabilitiesProvider;

impl CapabilitiesProvider for DummyCapabilitiesProvider {
    async fn acquire_capabilities(&self, capabilities: Capabilities) -> Capabilities {
        // Grant all capabilities that the widget asks for
        capabilities
    }
}

async fn run_test_driver(
    init_on_content_load: bool,
    is_room_e2ee: bool,
) -> (Client, MatrixMockServer, WidgetDriverHandle) {
    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    let room = mock_server.sync_joined_room(&client, &ROOM_ID).await;

    if is_room_e2ee {
        mock_server.mock_room_state_encryption().encrypted().mount().await;
    } else {
        mock_server.mock_room_state_encryption().plain().mount().await;
    }

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

async fn run_test_driver_e2e(
    init_on_content_load: bool,
) -> (Client, Client, MatrixMockServer, WidgetDriverHandle) {
    let mock_server = MatrixMockServer::new().await;
    mock_server.mock_crypto_endpoints_preset().await;
    let (alice, bob) = mock_server.set_up_alice_and_bob_for_encryption().await;

    let room = mock_server.sync_joined_room(&alice, &ROOM_ID).await;

    mock_server.mock_room_state_encryption().encrypted().mount().await;
    let (driver, handle) = WidgetDriver::new(
        WidgetSettings::new(WIDGET_ID.to_owned(), init_on_content_load, "https://foo.bar/widget")
            .unwrap(),
    );

    spawn(async move {
        if let Err(()) = driver.run(room, DummyCapabilitiesProvider).await {
            error!("An error encountered in running the WidgetDriver (no details available yet)");
        }
    });

    (alice, bob, mock_server, handle)
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
    println!("Json string sent from the widget {json_string}");
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
    let (_, _, driver_handle) = run_test_driver(false, false).await;

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

static HELLO_EVENT: Lazy<JsonValue> = Lazy::new(|| {
    json!({
        "content": {
            "body": "hello",
            "msgtype": "m.text",
        },
        "event_id": "$msda7m0df9E9op3",
        "origin_server_ts": 152037280,
        "sender": "@example:localhost",
        "type": "m.room.message",
        "room_id": &*ROOM_ID,
    })
});

static TOMBSTONE_EVENT: Lazy<JsonValue> = Lazy::new(|| {
    json!({
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
    })
});

#[async_test]
async fn test_read_messages() {
    let (_, mock_server, driver_handle) = run_test_driver(true, false).await;

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

    let response_json = json!({
        "chunk": [*HELLO_EVENT, *TOMBSTONE_EVENT],
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

#[async_test]
async fn test_read_messages_with_msgtype_capabilities() {
    let (_, mock_server, driver_handle) = run_test_driver(true, false).await;

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

async fn assert_state_synced(driver_handle: &WidgetDriverHandle, state: JsonValue) {
    let msg = recv_message(driver_handle).await;
    assert_eq!(msg["api"], "toWidget");
    assert_eq!(msg["action"], "update_state");
    assert_eq!(msg["data"]["state"], state);
}

#[async_test]
async fn test_read_room_members() {
    let (_, mock_server, driver_handle) = run_test_driver(false, false).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc2762.receive.state_event:m.room.member"]),
    )
    .await;

    // Wait for the state to be synced
    assert_state_synced(&driver_handle, json!([])).await;
    // No further messages from the driver yet
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);

    let f = EventFactory::new().room(&ROOM_ID);

    let leave_event = f
        .member(user_id!("@example:localhost"))
        .membership(MembershipState::Leave)
        .previous(MembershipState::Join)
        .into_raw_timeline();
    let join_event = f
        .member(user_id!("@example:localhost"))
        .membership(MembershipState::Join)
        .previous(MembershipState::Leave)
        .into_raw_timeline();

    let response_json = json!({
        "chunk": [*HELLO_EVENT, *TOMBSTONE_EVENT, leave_event, join_event],
        "end": "t47409-4357353_219380_26003_2269",
        "start": "t392-516_47314_0_7_1_1_1_11444_1"
    });
    mock_server
        .mock_room_messages()
        .match_limit(3)
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
            "type": "m.room.member",
            "state_key": true,
            "limit": 3,
        }),
    )
    .await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "org.matrix.msc2876.read_events");
    println!("{:?}", msg["response"]);
    let events = msg["response"]["events"].as_array().unwrap();

    // We should get both the leave event and the join event, because the
    // `read_events` action reads from the timeline, not the room state
    let [first_event, second_event]: &[_; 2] = events.as_slice().try_into().unwrap();
    assert_eq!(first_event, &leave_event.deserialize_as::<JsonValue>().unwrap());
    assert_eq!(second_event, &join_event.deserialize_as::<JsonValue>().unwrap());
}

#[async_test]
async fn test_receive_live_events() {
    let (client, mock_server, driver_handle) = run_test_driver(false, false).await;

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

    // Wait for the state to be synced
    assert_state_synced(&driver_handle, json!([])).await;
    // No further messages from the driver yet
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
async fn test_block_clear_to_device_in_e2ee_room() {
    let (client, mock_server, driver_handle) = run_test_driver(false, true).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.receive.to_device:my.custom.to.device"]),
    )
    .await;

    // No messages from the driver yet
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);

    mock_server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
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

    // The message should be filtered out because it is not encrypted and the room
    // is encrypted
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);
}

#[async_test]
async fn test_accept_encrypted_to_device_in_e2ee_room() {
    let (alice, bob, mock_server, driver_handle) = run_test_driver_e2e(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.receive.to_device:my.custom.to.device"]),
    )
    .await;

    let bob_alice_device = bob
        .encryption()
        .get_device(alice.user_id().unwrap(), alice.device_id().unwrap())
        .await
        .unwrap()
        .unwrap();

    let content_raw = Raw::new(&json!({
        "call_id": "",
    }))
    .unwrap()
    .cast_unchecked();

    let event_synced_future =
        mock_server.mock_capture_put_to_device_then_sync_back(bob.user_id().unwrap(), &alice).await;

    bob.encryption()
        .encrypt_and_send_raw_to_device(
            vec![&bob_alice_device],
            "my.custom.to.device",
            content_raw,
            CollectStrategy::AllDevices,
        )
        .await
        .unwrap();

    // No messages from the driver yet
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);

    event_synced_future.await;

    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "toWidget");
    assert_eq!(msg["action"], "send_to_device");

    let data = msg["data"].as_object().unwrap();
    assert_eq!(data["type"], "my.custom.to.device");
    assert_eq!(data["content"]["call_id"], "");
    assert_eq!(data["sender"], "@bob:example.org");
    // Only these 3 fields should be exposed to the widget (no
    // `sender_device_keys`/`keys`/..)
    assert_eq!(data.len(), 3);
}

/// Test that "internal" to-device messages are never forwarded to the widgets.
/// Here we test that a `m.room_key` message is never forwarded to the widget.
/// These events are already zeroized by the SDK, but let's not leak them
/// anyhow.
#[async_test]
async fn test_should_block_internal_to_device() {
    let (alice, bob, mock_server, driver_handle) = run_test_driver_e2e(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.receive.to_device:m.room_key"]),
    )
    .await;

    let room_id = room_id!("!room_id:localhost");

    let alice_member_state = json!({
        "content": {
            "avatar_url": null,
            "displayname": "Alice",
            "membership": "join"
        },
        "event_id": "$151800140517rfvjc:localhost",
        "membership": "join",
        "origin_server_ts": 151800140,
        "sender": alice.user_id().unwrap().to_string(),
        "state_key": alice.user_id().unwrap().to_string(),
        "type": "m.room.member",
        "unsigned": {
            "age": 297036,
        }
    });

    // Let's emulate what `MatrixMockServer::sync_joined_room()` does.
    mock_server
        .mock_sync()
        .ok_and_run(&bob, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id)
                    .add_state_event(StateTestEvent::Encryption)
                    .add_state_event(StateTestEvent::Custom(alice_member_state.clone())),
            );
        })
        .await;

    // Needed for the message to be sent in an encrypted room
    mock_server
        .mock_get_members()
        .ok(vec![serde_json::from_value(alice_member_state).unwrap()])
        .mock_once()
        .mount()
        .await;

    let (guard, room_key_event) =
        mock_server.mock_capture_put_to_device(bob.user_id().unwrap()).await;

    let msg = RoomMessageEventContent::text_plain("Hello world");
    let room_bob_pov = bob.get_room(room_id).unwrap();

    // We don't need to mock the room send end point for our test, just let the
    // event in queue the important part is the room key
    let _ = room_bob_pov.send(msg).await;

    // this is the room key event!
    let sent_event = room_key_event.await;
    drop(guard);

    // feed it back
    mock_server
        .mock_sync()
        .ok_and_run(&alice, |sync_builder| {
            sync_builder.add_to_device_event(sent_event.deserialize_as().unwrap());
        })
        .await;

    // the driver should block the message and the widget should see nothing
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);
}

#[async_test]
async fn test_receive_state() {
    let (client, mock_server, driver_handle) = run_test_driver(false, false).await;

    let f = EventFactory::new().room(&ROOM_ID);
    let name_event_1: Raw<AnySyncStateEvent> = f.room_name("room name").sender(&BOB).into();

    mock_server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_joined_room(
                // set room name - matches filter
                JoinedRoomBuilder::new(&ROOM_ID).add_state_event(name_event_1),
            );
        })
        .await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc2762.receive.state_event:m.room.name#"]),
    )
    .await;

    // Wait for the state to be synced
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "toWidget");
    assert_eq!(msg["action"], "update_state");
    assert_eq!(msg["data"]["state"].as_array().unwrap().len(), 1);
    assert_eq!(msg["data"]["state"][0]["type"], "m.room.name");
    assert_eq!(msg["data"]["state"][0]["room_id"], ROOM_ID.as_str());
    assert_eq!(msg["data"]["state"][0]["sender"], BOB.as_str());
    assert_eq!(msg["data"]["state"][0]["state_key"], "");
    assert_eq!(msg["data"]["state"][0]["content"]["name"], "room name");
    // No further messages from the driver yet
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);

    let topic_event: Raw<AnySyncStateEvent> = f.room_topic("new room topic").sender(&BOB).into();
    let name_event_2 = f.room_name("new room name").sender(&BOB);
    let name_event_3: Raw<AnySyncStateEvent> =
        f.room_name("even newer room name").sender(&BOB).into();

    mock_server
        .mock_sync()
        .ok_and_run(&client, |sync_builder| {
            sync_builder.add_joined_room(
                JoinedRoomBuilder::new(&ROOM_ID)
                    // text message from alice - doesn't match
                    .add_timeline_event(f.text_msg("simple text message").sender(&ALICE))
                    // set room topic - doesn't match
                    .add_timeline_event(topic_event.clone().cast())
                    .add_state_event(topic_event)
                    // set room name - matches filter but not reported in the state block
                    .add_timeline_event(name_event_2)
                    // set room name - matches filter
                    .add_timeline_event(name_event_3.clone().cast())
                    .add_state_event(name_event_3),
            );
        })
        .await;

    // Driver should have exactly 3 messages for us
    let msg1 = recv_message(&driver_handle).await;
    let msg2 = recv_message(&driver_handle).await;
    let msg3 = recv_message(&driver_handle).await;
    assert_matches!(recv_message(&driver_handle).now_or_never(), None);

    let (update_state, send_events): (Vec<_>, _) =
        [msg1, msg2, msg3].into_iter().partition(|msg| msg["action"] == "update_state");
    assert_eq!(update_state.len(), 1);
    assert_eq!(send_events.len(), 2);

    let msg = &update_state[0];
    assert_eq!(msg["api"], "toWidget");
    assert_eq!(msg["data"]["state"].as_array().unwrap().len(), 1);
    assert_eq!(msg["data"]["state"][0]["type"], "m.room.name");
    assert_eq!(msg["data"]["state"][0]["room_id"], ROOM_ID.as_str());
    assert_eq!(msg["data"]["state"][0]["sender"], BOB.as_str());
    assert_eq!(msg["data"]["state"][0]["state_key"], "");
    assert_eq!(msg["data"]["state"][0]["content"]["name"], "even newer room name");

    let msg = &send_events[0];
    assert_eq!(msg["api"], "toWidget");
    assert_eq!(msg["action"], "send_event");
    assert_eq!(msg["data"]["type"], "m.room.name");
    assert_eq!(msg["data"]["room_id"], ROOM_ID.as_str());
    assert_eq!(msg["data"]["sender"], BOB.as_str());
    assert_eq!(msg["data"]["state_key"], "");
    assert_eq!(msg["data"]["content"]["name"], "new room name");

    let msg = &send_events[1];
    assert_eq!(msg["api"], "toWidget");
    assert_eq!(msg["action"], "send_event");
    assert_eq!(msg["data"]["type"], "m.room.name");
    assert_eq!(msg["data"]["room_id"], ROOM_ID.as_str());
    assert_eq!(msg["data"]["sender"], BOB.as_str());
    assert_eq!(msg["data"]["state_key"], "");
    assert_eq!(msg["data"]["content"]["name"], "even newer room name");
}

#[async_test]
async fn test_send_room_message() {
    let (_, mock_server, driver_handle) = run_test_driver(false, false).await;

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
    let (_, mock_server, driver_handle) = run_test_driver(false, false).await;

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
    let (_, mock_server, driver_handle) = run_test_driver(false, false).await;

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
    let (_, mock_server, driver_handle) = run_test_driver(false, false).await;

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
    let (_, mock_server, driver_handle) = run_test_driver(false, false).await;

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
    let (_, _mock_server, driver_handle) = run_test_driver(false, false).await;

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
    let (_, mock_server, driver_handle) = run_test_driver(false, false).await;

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
    print!("{response:?}");
    assert_eq!(response["api"], "fromWidget");
    assert_eq!(response["action"], "org.matrix.msc4157.update_delayed_event");
    let empty_response = response["response"].clone();
    assert_eq!(empty_response, serde_json::from_str::<JsonValue>("{}").unwrap());
}

#[async_test]
async fn test_try_update_delayed_event_without_permission() {
    let (_, _mock_server, driver_handle) = run_test_driver(false, false).await;

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
    print!("{response:?}");
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
    let (_, _mock_server, driver_handle) = run_test_driver(false, false).await;

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
    let (_, mock_server, driver_handle) = run_test_driver(false, false).await;

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
    let (_, mock_server, driver_handle) = run_test_driver(false, false).await;

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
    let (alice, bob, mock_server, driver_handle) = run_test_driver_e2e(false).await;
    let carl = mock_server.set_up_carl_for_encryption(&alice, &bob).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.send.to_device:my.custom.to_device_type",]),
    )
    .await;

    let request_id = "0000000";

    let data = json!({
        "type": "my.custom.to_device_type",
        "messages": {
            bob.user_id().unwrap().to_string(): {
                bob.device_id().unwrap().to_string(): {
                    "param1":"test",
                },
            },
            carl.user_id().unwrap().to_string(): {
                carl.device_id().unwrap().to_string(): {
                    "param1":"test",
                },
            },
        }
    });

    let queue: Arc<std::sync::Mutex<PendingToDeviceMessages>> = Default::default();

    let guard =
        mock_server.capture_put_to_device_traffic(alice.user_id().unwrap(), queue.clone()).await;

    let bob_received: Arc<Mutex<(Option<AnyToDeviceEvent>, Option<EncryptionInfo>)>> =
        Default::default();

    bob.add_event_handler({
        let handled_event_info = bob_received.clone();
        move |ev: AnyToDeviceEvent, encryption_info: Option<EncryptionInfo>| {
            *handled_event_info.lock() = (Some(ev), encryption_info);
            future::ready(())
        }
    });

    let carl_received: Arc<Mutex<(Option<AnyToDeviceEvent>, Option<EncryptionInfo>)>> =
        Default::default();

    carl.add_event_handler({
        let handled_event_info = carl_received.clone();
        move |ev: AnyToDeviceEvent, encryption_info: Option<EncryptionInfo>| {
            *handled_event_info.lock() = (Some(ev), encryption_info);
            future::ready(())
        }
    });

    send_request(&driver_handle, request_id, "send_to_device", data).await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_to_device");
    let response = msg["response"].clone();
    assert_eq!(serde_json::to_string(&response).unwrap(), "{}");

    mock_server.sync_back_pending_to_device_messages(queue.clone(), &bob).await;

    mock_server.sync_back_pending_to_device_messages(queue.clone(), &carl).await;

    drop(guard);

    // Ensure bob received correctly
    {
        let (event, encryption_info) = bob_received.lock().clone();
        assert_let!(Some(event) = event);
        assert_eq!(event.event_type().to_string(), "my.custom.to_device_type");
        assert!(encryption_info.is_some());
    }

    // Ensure carl received correctly
    {
        let (event, encryption_info) = carl_received.lock().clone();
        assert_let!(Some(event) = event);
        assert_eq!(event.event_type().to_string(), "my.custom.to_device_type");
        assert!(encryption_info.is_some());
    }
}

#[async_test]
async fn test_send_encrypted_to_device_event_wildcard() {
    let (alice, bob, mock_server, driver_handle) = run_test_driver_e2e(false).await;

    let bob_2 = mock_server
        .set_up_new_device_for_encryption(&bob, device_id!("BOB2BOB2"), vec![&alice])
        .await;

    mock_server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_change_device(bob.user_id().unwrap());
        })
        .await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.send.to_device:my.custom.to_device_type",]),
    )
    .await;

    let request_id = "0000000";

    let data = json!({
        "type": "my.custom.to_device_type",
        "messages": {
            bob.user_id().unwrap().to_string(): {
                "*": {
                    "param1":"test",
                },
            },
        }
    });

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/.*/sendToDevice/m.room.encrypted/.*"))
        .respond_with(move |req: &Request| {
            // there should be two messages, one for bob and one for bob_2
            #[derive(Debug, serde::Deserialize)]
            struct Parameters {
                messages: Messages,
            }

            let params: Parameters = req.body_json().unwrap();
            assert_eq!(params.messages.len(), 1);
            let for_bob = params.messages.get(bob.user_id().unwrap()).unwrap();
            assert_eq!(for_bob.len(), 2);
            assert!(
                for_bob
                    .get(&DeviceIdOrAllDevices::DeviceId(bob.device_id().unwrap().to_owned()))
                    .is_some()
            );
            assert!(
                for_bob
                    .get(&DeviceIdOrAllDevices::DeviceId(bob_2.device_id().unwrap().to_owned()))
                    .is_some()
            );

            ResponseTemplate::new(200)
        })
        .expect(1)
        .mount(mock_server.server())
        .await;

    send_request(&driver_handle, request_id, "send_to_device", data).await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_to_device");
    let response = msg["response"].clone();
    assert_eq!(serde_json::to_string(&response).unwrap(), "{}");
}

/// Test the wildcard edge cases, like using mixed wildcard and explicit device
/// or when there are no devices at all. For now, we just log it and not report
/// errors.
#[async_test]
async fn test_send_encrypted_to_device_event_wildcard_edge_cases() {
    let (alice, bob, mock_server, driver_handle) = run_test_driver_e2e(false).await;

    let bob_2 = mock_server
        .set_up_new_device_for_encryption(&bob, device_id!("BOB2BOB2"), vec![&alice])
        .await;

    mock_server
        .mock_sync()
        .ok_and_run(&alice, |builder| {
            builder.add_change_device(bob.user_id().unwrap());
        })
        .await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.send.to_device:my.custom.to_device_type",]),
    )
    .await;

    let request_id = "0000000";

    let data = json!({
        "type": "my.custom.to_device_type",
        "messages": {
            bob.user_id().unwrap().to_string(): {
                "*": {
                    "param1":"test",
                },
                "OTHER_UNKNOWN": {
                    "param1":"test",
                },
            },
            "@carl:example.org": {
                "*": {
                    "param1":"test",
                },
            }
        }
    });

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/.*/sendToDevice/m.room.encrypted/.*"))
        .respond_with(move |req: &Request| {
            // there should be two messages, one for bob and one for bob_2
            #[derive(Debug, serde::Deserialize)]
            struct Parameters {
                messages: Messages,
            }

            let params: Parameters = req.body_json().unwrap();
            assert_eq!(params.messages.len(), 1);
            let for_bob = params.messages.get(bob.user_id().unwrap()).unwrap();
            assert_eq!(for_bob.len(), 2);
            assert!(
                for_bob
                    .get(&DeviceIdOrAllDevices::DeviceId(bob.device_id().unwrap().to_owned()))
                    .is_some()
            );
            assert!(
                for_bob
                    .get(&DeviceIdOrAllDevices::DeviceId(bob_2.device_id().unwrap().to_owned()))
                    .is_some()
            );

            ResponseTemplate::new(200)
        })
        .expect(1)
        .mount(mock_server.server())
        .await;

    send_request(&driver_handle, request_id, "send_to_device", data).await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_to_device");
    let response = msg["response"].clone();
    // For now we don't report unknown device when there is the wildcard
    assert_eq!(serde_json::to_string(&response).unwrap(), "{}");
}

#[async_test]
async fn test_send_encrypted_to_device_event_unknown_device() {
    let (_, _, driver_handle) = run_test_driver(false, true).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.send.to_device:my.custom.to_device_type",]),
    )
    .await;

    let request_id = "0000000";

    let data = json!({
        "type": "my.custom.to_device_type",
        "messages": {
            "@carl:example.org": {
                "UNKNOWN_DEVICE": {
                    "param1":"test",
                },
            },
        }
    });

    send_request(&driver_handle, request_id, "send_to_device", data).await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;

    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_to_device");
    let response = msg["response"].clone();
    assert_eq!(
        response,
        json!(
            {
                "failures": {
                    "@carl:example.org": ["UNKNOWN_DEVICE"]
                }
            }
        )
    );
}

#[async_test]
async fn test_send_internal_to_device_event() {
    let internal_types = vec![
        "m.dummy",
        "m.room_key",
        "m.room_key_request",
        "m.forwarded_room_key",
        "m.key.verification.request",
        "m.key.verification.ready",
        "m.key.verification.start",
        "m.key.verification.cancel",
        "m.key.verification.accept",
        "m.key.verification.key",
        "m.key.verification.mac",
        "m.key.verification.done",
        "m.secret.request",
        "m.secret.send",
        "m.room.encrypted",
    ];

    let (alice, bob, mock_server, driver_handle) = run_test_driver_e2e(false).await;

    let caps = internal_types
        .iter()
        .map(|internal_type| format!("org.matrix.msc3819.send.to_device:{internal_type}"))
        .collect::<Vec<String>>();

    negotiate_capabilities(&driver_handle, json!(caps)).await;

    let queue: Arc<std::sync::Mutex<PendingToDeviceMessages>> = Default::default();
    let guard =
        mock_server.capture_put_to_device_traffic(alice.user_id().unwrap(), queue.clone()).await;

    for internal_type in internal_types {
        let request_id = "0000000";

        let data = json!({
            "type": internal_type,
            "messages": {
                bob.user_id().unwrap().to_string(): {
                    bob.device_id().unwrap().to_string(): {
                        "foo":"bar",
                    },
                },
            }
        });

        send_request(&driver_handle, request_id, "send_to_device", data).await;

        // Receive the response
        let msg = recv_message(&driver_handle).await;

        assert_eq!(msg["api"], "fromWidget");
        assert_eq!(msg["action"], "send_to_device");
        let response = msg["response"].clone();
        assert_eq!(response, json!({}));
    }
    // This is silently dropped, but ensure no traffic is sent
    assert!(queue.lock().unwrap().is_empty());
    drop(guard);
}

#[async_test]
async fn test_send_encrypted_to_device_event_partial_error() {
    let (alice, bob, mock_server, driver_handle) = run_test_driver_e2e(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.send.to_device:my.custom.to_device_type",]),
    )
    .await;

    let request_id = "0000000";

    let data = json!({
        "type": "my.custom.to_device_type",
        "messages": {
            "@carl:example.org": {
                "UNKNOWN_DEVICE": {
                    "param1":"test",
                },
            },
            bob.user_id().unwrap().to_string(): {
                bob.device_id().unwrap().to_string(): {
                    "param1":"test",
                },
            },
        }
    });

    let (guard, event_as_sent_by_alice) =
        mock_server.mock_capture_put_to_device(alice.user_id().unwrap()).await;

    send_request(&driver_handle, request_id, "send_to_device", data).await;

    // It was sent to bob even though other recipients failed
    let event_as_sent_by_alice = event_as_sent_by_alice.await.deserialize().unwrap();
    drop(guard);
    assert_eq!(event_as_sent_by_alice.algorithm().as_str(), "m.olm.v1.curve25519-aes-sha2");

    // Receive the response
    let msg = recv_message(&driver_handle).await;

    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_to_device");
    let response = msg["response"].clone();
    assert_eq!(
        response,
        json!(
            {
                "failures": {
                    "@carl:example.org": ["UNKNOWN_DEVICE"]
                }
            }
        )
    );
}

#[async_test]
async fn test_send_encrypted_to_device_event_server_error() {
    let (_, bob, mock_server, driver_handle) = run_test_driver_e2e(false).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.send.to_device:my.custom.to_device_type",]),
    )
    .await;

    let request_id = "0000000";

    let data = json!({
        "type": "my.custom.to_device_type",
        "messages": {
            bob.user_id().unwrap().to_string(): {
                bob.device_id().unwrap().to_string(): {
                    "param1":"test",
                },
            },
        }
    });

    mock_server.mock_send_to_device().error500().mount().await;

    send_request(&driver_handle, request_id, "send_to_device", data).await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;

    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_to_device");
    let response = msg["response"].clone();
    assert_eq!(
        response,
        json!(
            {
                "failures": {
                    bob.user_id().unwrap().to_string(): [bob.device_id().unwrap().to_string()]
                }
            }
        )
    );
}

/// The existing widget-apis allows to encrypt and send different content per
/// device, ensure this works.
#[async_test]
async fn test_send_encrypted_to_device_different_content() {
    let (alice, bob, mock_server, driver_handle) = run_test_driver_e2e(false).await;

    let carl = mock_server.set_up_carl_for_encryption(&alice, &bob).await;

    negotiate_capabilities(
        &driver_handle,
        json!(["org.matrix.msc3819.send.to_device:my.custom.to_device_type",]),
    )
    .await;

    let request_id = "0000000";

    let data = json!({
        "type": "my.custom.to_device_type",
        "messages": {
             carl.user_id().unwrap().to_string(): {
                carl.device_id().unwrap().to_string(): {
                    "param1":"This for carl",
                    "param2": "val",
                },
            },
            bob.user_id().unwrap().to_string(): {
                bob.device_id().unwrap().to_string(): {
                    "param1":"This for bob",
                    "param2":"val",
                },
            },
        }
    });

    let queue: Arc<std::sync::Mutex<PendingToDeviceMessages>> = Default::default();

    let guard =
        mock_server.capture_put_to_device_traffic(alice.user_id().unwrap(), queue.clone()).await;

    let bob_received: HandledDeviceEventMutex = Default::default();

    bob.add_event_handler({
        let handled_event_info = bob_received.clone();
        move |ev: Raw<AnyToDeviceEvent>, encryption_info: Option<EncryptionInfo>| {
            *handled_event_info.lock() = (Some(ev), encryption_info);
            future::ready(())
        }
    });

    let carl_received: HandledDeviceEventMutex = Default::default();

    carl.add_event_handler({
        let handled_event_info = carl_received.clone();
        move |ev: Raw<AnyToDeviceEvent>, encryption_info: Option<EncryptionInfo>| {
            *handled_event_info.lock() = (Some(ev), encryption_info);
            future::ready(())
        }
    });

    send_request(&driver_handle, request_id, "send_to_device", data).await;

    // Receive the response
    let msg = recv_message(&driver_handle).await;

    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_to_device");
    let response = msg["response"].clone();
    assert_eq!(response, json!({}));

    mock_server.sync_back_pending_to_device_messages(queue.clone(), &bob).await;
    mock_server.sync_back_pending_to_device_messages(queue.clone(), &carl).await;

    drop(guard);

    // Ensure bob received correctly
    {
        let (event, encryption_info) = bob_received.lock().clone();
        assert_let!(Some(event) = event);
        assert!(encryption_info.is_some());

        let event = event.deserialize_as::<Value>().unwrap();
        assert_eq!(event["content"]["param1"], "This for bob");
    }

    // Ensure carl received correctly
    {
        let (event, encryption_info) = carl_received.lock().clone();
        assert_let!(Some(event) = event);
        assert!(encryption_info.is_some());

        let event = event.deserialize_as::<Value>().unwrap();
        assert_eq!(event["content"]["param1"], "This for carl");
    }
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
