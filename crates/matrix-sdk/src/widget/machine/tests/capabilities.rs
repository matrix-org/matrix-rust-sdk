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

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use ruma::owned_room_id;
use serde_json::{from_value, json};

use super::{parse_msg, WIDGET_ID};
use crate::widget::machine::{
    incoming::MatrixDriverResponse, Action, IncomingMessage, MatrixDriverRequestData, WidgetMachine,
};

#[test]
fn machine_can_negotiate_capabilities_immediately() {
    let room_id = owned_room_id!("!a98sd12bjh:example.org");
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id, false, None);
    assert_capabilities_dance(&mut machine, actions, None);
}

#[test]
fn machine_can_request_capabilities_on_content_load() {
    let room_id = owned_room_id!("!a98sd12bjh:example.org");
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id, true, None);
    assert!(actions.is_empty());

    // Content loaded event processed.
    let actions = {
        let mut actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
            "api": "fromWidget",
            "widgetId": WIDGET_ID,
            "requestId": "content-loaded-request-id",
            "action": "content_loaded",
            "data": {},
        })));

        let action = actions.remove(0);
        assert_let!(Action::SendToWidget(msg) = action);
        let (msg, request_id) = parse_msg(&msg);
        assert_eq!(request_id, "content-loaded-request-id");
        assert_eq!(
            msg,
            json!({
                "api": "fromWidget",
                "widgetId": WIDGET_ID,
                "action": "content_loaded",
                "data": {},
                "response": {},
            }),
        );

        actions
    };

    assert_capabilities_dance(&mut machine, actions, None);
}

#[test]
fn capabilities_failure_results_into_empty_capabilities() {
    let room_id = owned_room_id!("!a98sd12bjh:example.org");
    let (mut machine, actions) = WidgetMachine::new(WIDGET_ID.to_owned(), room_id, false, None);

    // Ask widget to provide desired capabilities.
    let actions = {
        let [action]: [Action; 1] = actions.try_into().unwrap();
        assert_let!(Action::SendToWidget(msg) = action);
        let (msg, request_id) = parse_msg(&msg);
        assert_eq!(
            msg,
            json!({
                "api": "toWidget",
                "widgetId": WIDGET_ID,
                "action": "capabilities",
                "data": {},
            }),
        );

        machine.process(IncomingMessage::WidgetMessage(json_string!({
            "api": "toWidget",
            "widgetId": WIDGET_ID,
            "requestId": request_id,
            "action": "capabilities",
            "data": {},
            "response": {
                "capabilities": ["org.matrix.msc2762.receive.state_event:m.room.member"],
            },
        })))
    };

    // Try to acquire capabilities by sending a request to a matrix driver.
    let actions = {
        let [action]: [Action; 1] = actions.try_into().unwrap();
        assert_let!(
            Action::MatrixDriverRequest {
                request_id,
                data: MatrixDriverRequestData::AcquireCapabilities(data)
            } = action
        );
        assert_eq!(
            data.desired_capabilities,
            from_value(json!(["org.matrix.msc2762.receive.state_event:m.room.member"])).unwrap()
        );

        machine.process(IncomingMessage::MatrixDriverResponse {
            request_id,
            response: Err("OHMG!".into()),
        })
    };

    // Inform the widget about the new capabilities, or lack of thereof :)
    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
    let (msg, _request_id) = parse_msg(&msg);
    assert_eq!(
        msg,
        json!({
            "api": "toWidget",
            "widgetId": WIDGET_ID,
            "action": "notify_capabilities",
            "data": {
                "requested": ["org.matrix.msc2762.receive.state_event:m.room.member"],
                "approved": [],
            },
        }),
    );
}

/// Performs a capability "dance", if no capability is specified, we assume that
/// it's: `org.matrix.msc2762.receive.state_event:m.room.member`.
pub(super) fn assert_capabilities_dance(
    machine: &mut WidgetMachine,
    actions: Vec<Action>,
    capability_str: Option<&str>,
) {
    let capability =
        capability_str.unwrap_or("org.matrix.msc2762.receive.state_event:m.room.member");

    // Ask widget to provide desired capabilities.
    let actions = {
        let [action]: [Action; 1] = actions.try_into().unwrap();
        assert_let!(Action::SendToWidget(msg) = action);
        let (msg, request_id) = parse_msg(&msg);
        assert_eq!(
            msg,
            json!({
                "api": "toWidget",
                "widgetId": WIDGET_ID,
                "action": "capabilities",
                "data": {},
            }),
        );

        machine.process(IncomingMessage::WidgetMessage(json_string!({
            "api": "toWidget",
            "widgetId": WIDGET_ID,
            "requestId": request_id,
            "action": "capabilities",
            "data": {},
            "response": {
                "capabilities": [capability],
            },
        })))
    };

    // Try to acquire capabilities by sending a request to a matrix driver.
    let mut actions = {
        let [action]: [Action; 1] = actions.try_into().unwrap();
        assert_let!(
            Action::MatrixDriverRequest {
                request_id,
                data: MatrixDriverRequestData::AcquireCapabilities(data)
            } = action
        );
        let capabilities = data.desired_capabilities;
        assert_eq!(capabilities, from_value(json!([capability])).unwrap());

        let response = Ok(MatrixDriverResponse::CapabilitiesAcquired(capabilities));
        let message = IncomingMessage::MatrixDriverResponse { request_id, response };
        machine.process(message)
    };

    // We get the `Subscribe` command if we requested some reading capabilities.
    if ["org.matrix.msc2762.receive.state_event", "org.matrix.msc2762.receive.event"]
        .into_iter()
        .any(|c| capability.starts_with(c))
    {
        let action = actions.remove(0);
        assert_matches!(action, Action::Subscribe);
    }

    // Inform the widget about the acquired capabilities.
    {
        let [action]: [Action; 1] = actions.try_into().unwrap();
        assert_let!(Action::SendToWidget(msg) = action);
        let (msg, request_id) = parse_msg(&msg);
        assert_eq!(
            msg,
            json!({
                "api": "toWidget",
                "widgetId": WIDGET_ID,
                "action": "notify_capabilities",
                "data": {
                    "requested": [capability],
                    "approved": [capability],
                },
            }),
        );

        let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
            "api": "toWidget",
            "widgetId": WIDGET_ID,
            "requestId": request_id,
            "action": "notify_capabilities",
            "data": {
                "requested": [capability],
                "approved": [capability],
            },
            "response": {},
        })));

        assert!(actions.is_empty());
    }
}
