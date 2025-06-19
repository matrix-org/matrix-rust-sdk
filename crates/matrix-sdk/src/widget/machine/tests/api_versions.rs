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
use serde_json::{Value as JsonValue, json};

use super::WIDGET_ID;
use crate::widget::machine::{Action, IncomingMessage, WidgetMachine};

#[test]
fn test_get_supported_api_versions() {
    let (mut machine, _) =
        WidgetMachine::new(WIDGET_ID.to_owned(), owned_room_id!("!a98sd12bjh:example.org"), true);

    let actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
        "api": "fromWidget",
        "widgetId": WIDGET_ID,
        "requestId": "S2ixNhjaC0kd0jJn",
        "action": "supported_api_versions",
        "data": {},
    })));

    let [action]: [Action; 1] = actions.try_into().unwrap();
    assert_let!(Action::SendToWidget(msg) = action);
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
                "supported_versions": [
                    "0.0.1",
                    "0.0.2",
                    "org.matrix.msc2762",
                    "org.matrix.msc2762_update_state",
                    "org.matrix.msc2871",
                    "org.matrix.msc3819",
                ]
            },
        }),
    );
}
