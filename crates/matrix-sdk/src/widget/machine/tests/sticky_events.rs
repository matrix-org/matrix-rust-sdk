// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use ruma::{OwnedRoomId, owned_room_id};
use serde_json::json;
use strass::assert_let;

use super::{WIDGET_ID, parse_msg};
use crate::widget::machine::{
    Action, IncomingMessage, MatrixDriverRequestData, WidgetMachine, incoming::MatrixDriverResponse,
};

const ROOM_ID: &str = "!a98sd12bjh:example.org";

fn room_id() -> OwnedRoomId {
    owned_room_id!("!a98sd12bjh:example.org")
}

/// Run the capability negotiation with the widget requesting, and the driver
/// approving, `capabilities`. Returns the actions following the approval.
fn negotiate_capabilities(
    machine: &mut WidgetMachine,
    actions: Vec<Action>,
    capabilities: &[&str],
) -> Vec<Action> {
    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(msg["action"], "capabilities");

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "toWidget",
        "widgetId": WIDGET_ID,
        "requestId": request_id,
        "action": "capabilities",
        "data": {},
        "response": { "capabilities": capabilities },
    })));

    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(
        Action::MatrixDriverRequest {
            request_id,
            data: MatrixDriverRequestData::AcquireCapabilities(data)
        } = action
    );

    machine.process(IncomingMessage::MatrixDriverResponse {
        request_id,
        response: Ok(MatrixDriverResponse::CapabilitiesAcquired(data.desired_capabilities)),
    })
}

/// Pop the trailing `notify_capabilities` action, leaving the others.
fn pop_notify_capabilities(actions: &mut Vec<Action>) {
    assert_let!(Some(Action::SendToWidget(msg)) = actions.pop());
    let (msg, _) = parse_msg(&msg);
    assert_eq!(msg["action"], "notify_capabilities");
}

/// The widget `send_event` request of a sticky `m.rtc.member` event.
fn send_sticky_event_message(state_key: Option<&str>) -> String {
    let mut data = json!({
        "content": { "msc4354_sticky_key": "slot", "application": "m.call" },
        "sticky_duration_ms": 5_000,
        "room_id": ROOM_ID,
        "type": "m.rtc.member",
    });
    if let Some(state_key) = state_key {
        data["state_key"] = state_key.into();
    }

    json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "send_event-request-id",
        "action": "send_event",
        "data": data,
    })
}

/// The error message of the single `send_event` response in `actions`.
fn error_message(actions: Vec<Action>) -> String {
    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(request_id, "send_event-request-id");
    assert_eq!(msg["action"], "send_event");
    msg["response"]["error"]["message"].as_str().unwrap().to_owned()
}

#[test]
fn test_sending_a_sticky_event_requires_the_capability() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions = negotiate_capabilities(
        &mut machine,
        actions,
        &["org.matrix.msc2762.send.event:m.rtc.member"],
    );
    pop_notify_capabilities(&mut actions);
    assert!(actions.is_empty());

    let actions = machine.process(IncomingMessage::WidgetMessage(send_sticky_event_message(None)));
    assert!(error_message(actions).contains("org.matrix.msc4407.send.sticky_event"));
}

#[test]
fn test_sending_a_sticky_event_requires_a_send_filter() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions =
        negotiate_capabilities(&mut machine, actions, &["org.matrix.msc4407.send.sticky_event"]);
    pop_notify_capabilities(&mut actions);
    assert!(actions.is_empty());

    let actions = machine.process(IncomingMessage::WidgetMessage(send_sticky_event_message(None)));
    assert_eq!(error_message(actions), "Not allowed to send event");
}

#[test]
fn test_a_state_event_cannot_be_sticky() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions = negotiate_capabilities(
        &mut machine,
        actions,
        &[
            "org.matrix.msc2762.send.state_event:m.rtc.member",
            "org.matrix.msc4407.send.sticky_event",
        ],
    );
    pop_notify_capabilities(&mut actions);
    assert!(actions.is_empty());

    let actions = machine.process(IncomingMessage::WidgetMessage(send_sticky_event_message(Some(
        "@alice:example.org",
    ))));
    assert_eq!(error_message(actions), "Cannot send a state event with a sticky duration");
}

#[test]
fn test_sending_a_sticky_event_reaches_the_driver() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions = negotiate_capabilities(
        &mut machine,
        actions,
        &["org.matrix.msc2762.send.event:m.rtc.member", "org.matrix.msc4407.send.sticky_event"],
    );
    pop_notify_capabilities(&mut actions);
    assert!(actions.is_empty());

    let actions = machine.process(IncomingMessage::WidgetMessage(send_sticky_event_message(None)));
    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(
        Action::MatrixDriverRequest { data: MatrixDriverRequestData::SendEvent(request), .. } =
            action
    );
    assert_eq!(request.event_type, "m.rtc.member");
    assert_eq!(request.sticky_duration_ms, Some(5_000));
    assert!(request.delay.is_none());
}

#[test]
fn test_a_redaction_cannot_be_sticky() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions = negotiate_capabilities(
        &mut machine,
        actions,
        &["org.matrix.msc2762.send.event:m.room.redaction", "org.matrix.msc4407.send.sticky_event"],
    );
    pop_notify_capabilities(&mut actions);
    assert!(actions.is_empty());

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "send_event-request-id",
        "action": "send_event",
        "data": {
            "content": { "redacts": "$alice:example.org" },
            "sticky_duration_ms": 5_000,
            "room_id": ROOM_ID,
            "type": "m.room.redaction",
        },
    })));
    assert_eq!(error_message(actions), "Cannot send a redaction with a sticky duration");
}
