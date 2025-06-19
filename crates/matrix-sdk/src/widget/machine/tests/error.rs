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

use assert_matches2::assert_let;
use ruma::owned_room_id;
use serde_json::json;

use super::{WIDGET_ID, capabilities::assert_capabilities_dance, parse_msg};
use crate::widget::machine::{Action, IncomingMessage, WidgetMachine};

#[test]
fn test_machine_sends_error_for_unknown_request() {
    let room_id = owned_room_id!("!a98sd12bjh:example.org");
    let (mut machine, _) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id, true);

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "invalid-req",
        "action": "I AM ERROR",
        "data": {
            "some": "field",
        },
    })));

    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(request_id, "invalid-req");
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["widgetId"], WIDGET_ID);
    assert_eq!(msg["action"], "I AM ERROR");
    assert_eq!(msg["data"], json!({ "some": "field" }));
    assert!(msg["response"]["error"]["message"].is_string());
}

#[test]
fn test_read_messages_without_capabilities() {
    let (mut machine, _) =
        WidgetMachine::new(WIDGET_ID.to_owned(), owned_room_id!("!a98sd12bjh:example.org"), true);

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "get-me-some-messages",
        "action": "org.matrix.msc2876.read_events",
        "data": {
            "type": "m.room.message",
        },
    })));

    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(request_id, "get-me-some-messages");
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "org.matrix.msc2876.read_events");
    assert_eq!(
        msg["response"]["error"]["message"].as_str().unwrap(),
        "Received read event request before capabilities were negotiated"
    );
}

#[test]
fn test_read_request_for_non_allowed_message_like_events() {
    let room_id = owned_room_id!("!a98sd12bjh:example.org");
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id, false);
    assert_capabilities_dance(&mut machine, actions, None);

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "get-me-some-messages",
        "action": "org.matrix.msc2876.read_events",
        "data": {
            "type": "m.room.message",
        },
    })));

    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(request_id, "get-me-some-messages");
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "org.matrix.msc2876.read_events");
    assert_eq!(
        msg["response"]["error"]["message"].as_str().unwrap(),
        "Not allowed to read message-like event"
    );
}

#[test]
fn test_read_request_for_non_allowed_state_events() {
    let room_id = owned_room_id!("!a98sd12bjh:example.org");
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id, false);
    assert_capabilities_dance(&mut machine, actions, None);

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "get-me-some-messages",
        "action": "org.matrix.msc2876.read_events",
        "data": {
            "type": "m.room.topic",
            "state_key": true,
        },
    })));

    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(request_id, "get-me-some-messages");
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "org.matrix.msc2876.read_events");
    assert_eq!(
        msg["response"]["error"]["message"].as_str().unwrap(),
        "Not allowed to read state event"
    );
}

#[test]
fn test_send_request_for_non_allowed_state_events() {
    let room_id = owned_room_id!("!a98sd12bjh:example.org");
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id, false);
    assert_capabilities_dance(
        &mut machine,
        actions,
        Some("org.matrix.msc2762.send.state_event:m.room.member"),
    );

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "send-me-a-message",
        "action": "send_event",
        "data": {
            "type": "m.room.topic",
            "content": {
                "topic": "Hello world",
            },
        },
    })));

    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(request_id, "send-me-a-message");
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_event");
    assert_eq!(msg["response"]["error"]["message"].as_str().unwrap(), "Not allowed to send event");
}

#[test]
fn test_send_request_for_non_allowed_message_like_events() {
    let room_id = owned_room_id!("!a98sd12bjh:example.org");
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id, false);
    assert_capabilities_dance(
        &mut machine,
        actions,
        Some("org.matrix.msc2762.send.event:m.room.message#m.text"),
    );

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "send-me-a-message",
        "action": "send_event",
        "data": {
            "type": "m.room.message",
            "content": {
                "msgtype": "m.custom",
            },
        },
    })));

    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(request_id, "send-me-a-message");
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "send_event");
    assert_eq!(msg["response"]["error"]["message"].as_str().unwrap(), "Not allowed to send event");
}

#[test]
fn test_read_request_for_message_like_with_disallowed_msg_type_fails() {
    let room_id = owned_room_id!("!a98sd12bjh:example.org");
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id, false);
    assert_capabilities_dance(
        &mut machine,
        actions,
        Some("org.matrix.msc2762.receive.event:m.room.message#m.text"),
    );

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "get-me-some-messages",
        "action": "org.matrix.msc2876.read_events",
        "data": {
            "type": "m.reaction",
            "limit": 1,
        },
    })));

    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(request_id, "get-me-some-messages");
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["action"], "org.matrix.msc2876.read_events");
    assert_eq!(
        msg["response"]["error"]["message"].as_str().unwrap(),
        "Not allowed to read message-like event"
    );
}
