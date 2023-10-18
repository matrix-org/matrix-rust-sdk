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
use serde_json::json;

use super::{parse_msg, WIDGET_ID};
use crate::widget::machine::{Action, IncomingMessage, WidgetMachine};

#[test]
fn machine_sends_error_for_unknown_request() {
    let (mut machine, mut actions_recv) = WidgetMachine::new(WIDGET_ID.to_owned(), true);

    // No messages from the machine at first
    assert_matches!(actions_recv.try_recv(), Err(_));

    machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "invalid-req",
        "action": "I AM ERROR",
        "data": {
            "some": "field",
        },
    })));

    let action = actions_recv.try_recv().unwrap();
    let msg = assert_matches!(action, Action::SendToWidget(msg) => msg);
    let (msg, request_id) = parse_msg(&msg);
    assert_eq!(request_id, "invalid-req");
    assert_eq!(msg["api"], "fromWidget");
    assert_eq!(msg["widgetId"], WIDGET_ID);
    assert_eq!(msg["action"], "I AM ERROR");
    assert_eq!(msg["data"], json!({ "some": "field" }));
    assert!(msg["response"]["error"]["message"].is_string());
}
