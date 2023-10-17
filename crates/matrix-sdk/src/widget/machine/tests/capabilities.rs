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
use ruma::serde::JsonObject;
use serde_json::{json, Value as JsonValue};

use crate::widget::machine::{Action, WidgetMachine};

const WIDGET_ID: &str = "test-widget";

fn assert_msg_eq(msg: &str, value: JsonValue) {
    let object = value.as_object().unwrap();
    let mut deserialized: JsonObject = serde_json::from_str(msg).unwrap();

    // Messages must have a request ID, but we only want to test its value in
    // some rare cases.
    if !object.contains_key("requestId") {
        let request_id = deserialized.remove("requestId").unwrap();
        assert!(request_id.is_string());
    }

    assert_eq!(JsonValue::Object(deserialized), value);
}

#[test]
fn machine_can_request_capabilities_immediately() {
    let (_machine, mut actions_recv) = WidgetMachine::new(WIDGET_ID.to_owned(), false);
    let action = actions_recv.try_recv().unwrap();
    let msg = assert_matches!(action, Action::SendToWidget(msg) => msg);
    assert_msg_eq(
        &msg,
        json!({
            "api": "toWidget",
            "widgetId": WIDGET_ID,
            "action": "capabilities",
            "data": {},
        }),
    );

    assert_matches!(actions_recv.try_recv(), Err(_));
}

#[test]
fn machine_can_request_capabilities_on_content_load() {
    let (_machine, mut actions_recv) = WidgetMachine::new(WIDGET_ID.to_owned(), true);
    assert_matches!(actions_recv.try_recv(), Err(_));

    // TODO: Do the actual content load dance
}
