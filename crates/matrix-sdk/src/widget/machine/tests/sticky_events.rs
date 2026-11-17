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

use assert_matches::assert_matches;
use ruma::{OwnedRoomId, events::AnyTimelineEvent, owned_room_id, serde::Raw};
use serde_json::{Value as JsonValue, json};
use strass::assert_let;

use super::{WIDGET_ID, parse_msg};
use crate::widget::machine::{
    Action, IncomingMessage, MatrixDriverRequestData, WidgetMachine, incoming::MatrixDriverResponse,
};

const ROOM_ID: &str = "!a98sd12bjh:example.org";

fn room_id() -> OwnedRoomId {
    owned_room_id!("!a98sd12bjh:example.org")
}

/// A sticky `m.rtc.member` event of `sender`.
fn sticky_member_event(event_id: &str, sender: &str) -> JsonValue {
    json!({
        "type": "m.rtc.member",
        "event_id": event_id,
        "sender": sender,
        "origin_server_ts": 1_000,
        "room_id": ROOM_ID,
        "content": { "msc4354_sticky_key": "slot", "application": "m.call" },
        "msc4354_sticky": { "duration_ms": 600_000 },
    })
}

fn raw_event(event: &JsonValue) -> Raw<AnyTimelineEvent> {
    serde_json::from_value(event.clone()).unwrap()
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
fn test_live_sticky_events_are_pushed_once_the_capability_is_granted() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions = negotiate_capabilities(
        &mut machine,
        actions,
        &[
            "org.matrix.msc2762.receive.event:m.rtc.member",
            "org.matrix.msc4407.receive.sticky_event",
        ],
    );

    pop_notify_capabilities(&mut actions);
    let [subscribe, read_sticky]: [Action; 2] = actions.try_into().unwrap();
    assert_matches!(subscribe, Action::Subscribe);
    assert_let!(
        Action::MatrixDriverRequest {
            request_id,
            data: MatrixDriverRequestData::ReadStickyEvents
        } = read_sticky
    );

    // The driver answers with a sticky event the widget may read, and one it
    // may not.
    let alice = sticky_member_event("$alice", "@alice:example.org");
    let mut message = sticky_member_event("$bob", "@bob:example.org");
    message["type"] = "m.room.message".into();
    let actions = machine.process(IncomingMessage::MatrixDriverResponse {
        request_id,
        response: Ok(MatrixDriverResponse::EventsRead(vec![
            raw_event(&alice),
            raw_event(&message),
        ])),
    });

    // Only the allowed one is pushed.
    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, _) = parse_msg(&msg);
    assert_eq!(msg["action"], "send_event");
    assert_eq!(msg["data"], alice);
}

#[test]
fn test_live_sticky_events_are_not_read_with_only_state_read_filters() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions = negotiate_capabilities(
        &mut machine,
        actions,
        &[
            "org.matrix.msc2762.receive.state_event:m.room.member",
            "org.matrix.msc4407.receive.sticky_event",
        ],
    );

    // Sticky events are message-like. Only the initial room state is read.
    pop_notify_capabilities(&mut actions);
    let [subscribe, read_state]: [Action; 2] = actions.try_into().unwrap();
    assert_matches!(subscribe, Action::Subscribe);
    assert_matches!(
        read_state,
        Action::MatrixDriverRequest { data: MatrixDriverRequestData::ReadState(_), .. }
    );
}

#[test]
fn test_live_sticky_events_are_not_read_without_the_capability() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions = negotiate_capabilities(
        &mut machine,
        actions,
        &["org.matrix.msc2762.receive.event:m.rtc.member"],
    );

    pop_notify_capabilities(&mut actions);
    let [subscribe]: [Action; 1] = actions.try_into().unwrap();
    assert_matches!(subscribe, Action::Subscribe);
}

#[test]
fn test_live_sticky_events_are_not_read_without_a_read_filter() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions =
        negotiate_capabilities(&mut machine, actions, &["org.matrix.msc4407.receive.sticky_event"]);

    pop_notify_capabilities(&mut actions);
    assert!(actions.is_empty());
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

#[test]
fn test_a_failed_read_of_the_live_sticky_events_pushes_nothing() {
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id(), false);
    let mut actions = negotiate_capabilities(
        &mut machine,
        actions,
        &[
            "org.matrix.msc2762.receive.event:m.rtc.member",
            "org.matrix.msc4407.receive.sticky_event",
        ],
    );

    pop_notify_capabilities(&mut actions);
    let [_subscribe, read_sticky]: [Action; 2] = actions.try_into().unwrap();
    assert_let!(
        Action::MatrixDriverRequest {
            request_id,
            data: MatrixDriverRequestData::ReadStickyEvents
        } = read_sticky
    );

    let actions = machine.process(IncomingMessage::MatrixDriverResponse {
        request_id,
        response: Err(crate::Error::UnknownError("OHMG!".into())),
    });
    assert!(actions.is_empty());
}
