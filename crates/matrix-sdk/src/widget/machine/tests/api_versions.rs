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
use serde_json::{json, Value as JsonValue};

use super::WIDGET_ID;
use crate::widget::machine::{Action, IncomingMessage, WidgetMachine};

#[test]
fn get_supported_api_versions() {
    let (mut machine, mut actions_recv) = WidgetMachine::new(WIDGET_ID.to_owned(), true);

    machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "S2ixNhjaC0kd0jJn",
        "action": "supported_api_versions",
        "data": {},
    })));

    let action = actions_recv.try_recv().unwrap();
    let msg = assert_matches!(action, Action::SendToWidget(msg) => msg);
    let msg: JsonValue = serde_json::from_str(&msg).unwrap();
    assert_eq!(
        msg,
        json!({
            "api": "fromWidget",
            "widgetId": WIDGET_ID,
            "requestId": "S2ixNhjaC0kd0jJn",
            "action": "supported_api_versions",
            "data": {},
            "response": {
                "versions": [
                    "0.0.1",
                    "0.0.2",
                    "org.matrix.msc2762",
                    "org.matrix.msc2871",
                    "org.matrix.msc3819",
                ]
            },
        }),
    );
}
