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

//! Client widget API implementation.

#![warn(unreachable_pub)]

use std::sync::Arc;

use serde_json::{from_str as from_json, json};
use tracing::warn;

pub(crate) use self::matrix::Driver as MatrixDriver;
use self::{handler::MessageHandler, widget::WidgetProxy};
use super::{
    messages::{Action, Message},
    PermissionsProvider, WidgetDriver,
};

mod handler;
mod matrix;
mod widget;

/// Runs the client widget API handler for a given widget with a provided
/// `client`. Returns once the widget is disconnected.
pub(super) async fn run<T: PermissionsProvider>(
    client: MatrixDriver<T>,
    WidgetDriver { settings, from_widget_rx, to_widget_tx }: WidgetDriver,
) {
    // A small proxy object to interact with a widget via high-level API.
    let widget = Arc::new(WidgetProxy::new(settings, to_widget_tx));

    // Create a message handler (handles incoming requests from the widget).
    let handler = MessageHandler::new(client, widget.clone());

    // Receive a plain JSON message from a widget and parse it.
    while let Ok(raw) = from_widget_rx.recv().await {
        match from_json::<Message>(&raw) {
            // The message is valid, process it.
            Ok(msg) => match msg.action {
                // This is an incoming request from a widget.
                Action::FromWidget(action) => handler.handle(msg.header, action).await,
                // This is a response to our (outgoing) request.
                Action::ToWidget(action) => widget.handle_widget_response(msg.header, action).await,
            },
            // The message has an invalid format, report an error.
            Err(e) => {
                if let Ok(message) = from_json::<serde_json::Value>(&raw) {
                    match message["response"] {
                        serde_json::Value::Null => {
                            widget.send_error(Some(message), e.to_string()).await;
                        }
                        serde_json::Value::Number(_)
                        | serde_json::Value::String(_)
                        | serde_json::Value::Object(_) => {
                            warn!("ERROR parsing response");
                            //This cannot be send to the widget as a response, because it already
                            // contains a response field
                            widget
                                .send_error(Some(json!({"widget_id": widget.id()})), e.to_string())
                                .await;
                        }
                        _ => {}
                    }
                } else {
                    widget
                        .send_error(
                            None,
                            "The request json could not be parsed as json. Its malformatted.",
                        )
                        .await;
                }
            }
        }
    }
}
