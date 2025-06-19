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

use assert_matches2::assert_let;
use ruma::{
    ServerName, api::client::account::request_openid_token, authentication::TokenType,
    owned_room_id,
};
use serde_json::json;

use super::{WIDGET_ID, parse_msg};
use crate::widget::machine::{
    Action, IncomingMessage, MatrixDriverRequestData, MatrixDriverResponse, WidgetMachine,
};

#[test]
fn test_openid_request_handling_works() {
    let (mut machine, _) =
        WidgetMachine::new(WIDGET_ID.to_owned(), owned_room_id!("!a98sd12bjh:example.org"), true);

    // Widget requests an open ID token, since we don't have any caching yet,
    // we reply with a pending response right away.
    let actions = {
        let mut actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
            "api": "fromWidget",
            "widgetId": WIDGET_ID,
            "requestId": "openid-request-id",
            "action": "get_openid",
            "data": {},
        })));

        let action = actions.remove(0);
        assert_let!(Action::SendToWidget(msg) = action);
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

        actions
    };

    // Then we send an OpenID request to the driver and expect an answer.
    let actions = {
        let [action]: [Action; 1] = actions.try_into().unwrap();
        assert_let!(
            Action::MatrixDriverRequest { request_id, data: MatrixDriverRequestData::GetOpenId } =
                action
        );

        machine.process(IncomingMessage::MatrixDriverResponse {
            request_id,
            response: Ok(MatrixDriverResponse::OpenIdReceived(
                request_openid_token::v3::Response::new(
                    "access_token".to_owned(),
                    TokenType::Bearer,
                    ServerName::parse("example.org").unwrap(),
                    Duration::from_secs(3600),
                ),
            )),
        })
    };

    // We inform the widget about the new OpenID token.
    {
        let [action]: [Action; 1] = actions.try_into().unwrap();
        assert_let!(Action::SendToWidget(msg) = action);
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
}

#[test]
fn test_openid_fail_results_in_response_blocked() {
    let (mut machine, _) =
        WidgetMachine::new(WIDGET_ID.to_owned(), owned_room_id!("!a98sd12bjh:example.org"), true);

    // Widget requests an open ID token, since we don't have any caching yet,
    // we reply with a pending response right away.
    let mut actions = {
        let mut actions = machine.process(IncomingMessage::WidgetMessage(json_string!({
            "api": "fromWidget",
            "widgetId": WIDGET_ID,
            "requestId": "openid-request-id",
            "action": "get_openid",
            "data": {},
        })));

        let action = actions.remove(0);
        assert_let!(Action::SendToWidget(msg) = action);
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

        actions
    };

    // Then we send an OpenID request to the driver and expect a fail.
    let mut actions = {
        let action = actions.remove(0);
        assert!(actions.is_empty());
        assert_let!(
            Action::MatrixDriverRequest { request_id, data: MatrixDriverRequestData::GetOpenId } =
                action
        );

        machine.process(IncomingMessage::MatrixDriverResponse {
            request_id,
            response: Err(crate::Error::UnknownError("Unlucky one".into())),
        })
    };

    // We inform the widget about the new OpenID token.
    {
        let action = actions.remove(0);
        assert_let!(Action::SendToWidget(msg) = action);
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
    assert!(actions.is_empty());
}
