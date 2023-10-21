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
use ruma::{api::client::account::request_openid_token, owned_room_id, ServerName};
use serde_json::json;

use super::{parse_msg, WIDGET_ID};
use crate::widget::machine::{
    Action, IncomingMessage, MatrixDriverRequestData, MatrixDriverResponse, WidgetMachine,
};

#[test]
fn openid_request_handling_works() {
    let (mut machine, mut actions_recv) =
        WidgetMachine::new(WIDGET_ID.to_owned(), owned_room_id!("!a98sd12bjh:example.org"), true);

    // Widget requests an open ID token, since we don't have any caching yet,
    // we reply with a pending response right away.
    {
        machine.process(IncomingMessage::WidgetMessage(json_string!({
            "api": "fromWidget",
            "widgetId": WIDGET_ID,
            "requestId": "openid-request-id",
            "action": "get_openid",
            "data": {},
        })));

        let action = actions_recv.try_recv().unwrap();
        let msg = assert_matches!(action, Action::SendToWidget(msg) => msg);
        let (msg, request_id) = parse_msg(&msg);
        assert_eq!(request_id, "openid-request-id");
        assert_eq!(
            msg,
            json!({
                "api": "fromWidget",
                "widgetId": WIDGET_ID,
                "action": "get_openid",
                "data": {},
                "response": {
                    "state": "request",
                },
            }),
        );
    }

    // Then we send an OpenID request to the driver and expect an answer.
    {
        let action = actions_recv.try_recv().unwrap();
        let request_id = assert_matches!(
            action,
            Action::MatrixDriverRequest {
                request_id,
                data: MatrixDriverRequestData::GetOpenId,
            } => request_id
        );

        machine.process(IncomingMessage::MatrixDriverResponse {
            request_id,
            response: Ok(MatrixDriverResponse::OpenIdReceived(
                request_openid_token::v3::Response::new(
                    "access_token".to_owned(),
                    "Bearer".try_into().unwrap(),
                    ServerName::parse("example.org").unwrap().to_owned(),
                    Duration::from_secs(3600),
                ),
            )),
        });
    }

    // We inform the widget about the new OpenID token.
    {
        let action = actions_recv.try_recv().unwrap();
        let msg = assert_matches!(action, Action::SendToWidget(msg) => msg);
        let (msg, _request_id) = parse_msg(&msg);
        assert_eq!(
            msg,
            json!({
                "api": "toWidget",
                "widgetId": WIDGET_ID,
                "action": "openid_credentials",
                "data": {
                    "state": "allowed",
                    "original_request_id": "openid-request-id",
                    "access_token": "access_token",
                    "token_type": "Bearer",
                    "matrix_server_name": "example.org",
                    "expires_in": 3600,
                },
            }),
        );
    }

    // No further actions expected.
    assert_matches!(actions_recv.try_recv(), Err(_));
}

#[test]
fn openid_fail_results_in_response_blocked() {
    let (mut machine, mut actions_recv) =
        WidgetMachine::new(WIDGET_ID.to_owned(), owned_room_id!("!a98sd12bjh:example.org"), true);

    // Widget requests an open ID token, since we don't have any caching yet,
    // we reply with a pending response right away.
    {
        machine.process(IncomingMessage::WidgetMessage(json_string!({
            "api": "fromWidget",
            "widgetId": WIDGET_ID,
            "requestId": "openid-request-id",
            "action": "get_openid",
            "data": {},
        })));

        let action = actions_recv.try_recv().unwrap();
        let msg = assert_matches!(action, Action::SendToWidget(msg) => msg);
        let (msg, request_id) = parse_msg(&msg);
        assert_eq!(request_id, "openid-request-id");
        assert_eq!(
            msg,
            json!({
                "api": "fromWidget",
                "widgetId": WIDGET_ID,
                "action": "get_openid",
                "data": {},
                "response": {
                    "state": "request",
                },
            }),
        );
    }

    // Then we send an OpenID request to the driver and expect a fail.
    {
        let action = actions_recv.try_recv().unwrap();
        let request_id = assert_matches!(
            action,
            Action::MatrixDriverRequest {
                request_id,
                data: MatrixDriverRequestData::GetOpenId,
            } => request_id
        );

        machine.process(IncomingMessage::MatrixDriverResponse {
            request_id,
            response: Err("Unlucky one".into()),
        });
    }

    // We inform the widget about the new OpenID token.
    {
        let action = actions_recv.try_recv().unwrap();
        let msg = assert_matches!(action, Action::SendToWidget(msg) => msg);
        let (msg, _request_id) = parse_msg(&msg);
        assert_eq!(
            msg,
            json!({
                "api": "toWidget",
                "widgetId": WIDGET_ID,
                "action": "openid_credentials",
                "data": {
                    "state": "blocked",
                    "original_request_id": "openid-request-id",
                },
            }),
        );
    }

    // No further actions expected.
    assert_matches!(actions_recv.try_recv(), Err(_));
}
