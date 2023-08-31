//! Client widget API implementation.

#![warn(unreachable_pub)]

use std::{
    collections::HashMap,
    result::Result as StdResult,
    sync::{Arc, Mutex},
};

use async_channel::Sender;
use serde_json::{from_str as from_json, json, to_string as to_json};
use tokio::sync::oneshot;
use tracing::warn;
use uuid::Uuid;

use self::handler::{IncomingResponse, MessageHandler, OutgoingRequest, OutgoingResponse};
pub(crate) use self::{
    handler::{Error, Result},
    matrix::Driver as MatrixDriver,
};
use super::{
    messages::{self, to_widget::Action as ToWidgetAction, Action, BadMessage, Header, Message},
    PermissionsProvider, Widget, WidgetSettings as WidgetInfo,
};

mod handler;
mod matrix;

/// Runs the client widget API handler for a given widget with a provided
/// `client`. Returns once the widget is disconnected or some terminal error
/// occurs.
pub(super) async fn run<T: PermissionsProvider>(
    client: MatrixDriver<T>,
    widget: Widget,
) -> Result<()> {
    // Clone id for potential error message
    let widget_id = widget.settings.id.clone();
    // A map of outgoing requests that we are waiting a response for, i.e. a
    // requests that we sent to the widget and that we're expecting an answer
    // from.
    let state: PendingResponses = Arc::new(Mutex::new(HashMap::new()));

    // Create a message handler (handles incoming requests from the widget).
    let handler = {
        let widget = WidgetProxy::new(widget.settings, widget.comm.to.clone(), state.clone());
        MessageHandler::new(client, widget)
    };

    // Receives plain JSON string messages from a widget, parses it and passes
    // it to the handler.
    while let Ok(raw) = widget.comm.from.recv().await {
        let handle_result = match from_json::<Message>(&raw) {
            Ok(msg) => match msg.action {
                // This is an incoming request from a widget.
                Action::FromWidget(action) => handler.handle(msg.header, action).await,
                // This is a response from the widget to a request from the
                // client / driver (i.e. response to the outgoing request from
                // our perspective).
                Action::ToWidget(action) => {
                    let pending = state
                        .lock()
                        .expect("Pending mutex poisoned")
                        .remove(&msg.header.request_id);

                    if let Some(tx) = pending {
                        // It's ok if send fails here, it just means that the
                        // widget has disconnected.
                        let _ = tx.send(action);
                    };
                    Ok(())

                    // TODO: We don't seem to have an error response for the
                    // unexpected **responses** to our requests. Only for errors
                    // that occur during the processing of the **incoming
                    // requests**, not the **outgoing requests** (requests sent
                    // by us **to** the widget). Clarify that later.
                }
            },
            Err(e) => {
                // Parse the message as a BadMessage
                let mut issues: Vec<String> = vec![];
                match from_json::<BadMessage>(&raw) {
                    Ok(bad_msg) => issues.push(format!("{}", bad_msg)),
                    Err(_) => issues.push("The message could not be parsed. There most likely is a json syntax issue in the sent request.".to_owned()),
                }
                issues.push(format!("\nThe message string:\n{}", &raw));
                issues.push(format!("\nAdditional error description:\n{}", e));
                let error_message = issues.join("");
                warn!("Failed to parse message from widget:\n{}", error_message);
                Err(Error::custom(error_message))
            }
        };

        // Send a message to the widget with any error that occured during the handling
        // process.
        if let Err(error) = handle_result {
            // build error body
            let error_body = messages::ErrorBody::new(error.to_string());

            // try to build a msg out of the raw string or build a minimal request
            let mut value_msg: serde_json::Value =
                from_json(&raw).unwrap_or(json!({ "widgetId": widget_id }));

            // set the response on the error message
            value_msg["response"] = serde_json::to_value(&error_body)
                .ok()
                .unwrap_or("Could not construct error object".into());

            // last send the message with the error message body to the widget.
            let _send_error = widget.comm.to.send_blocking(value_msg.to_string());
        }
    }

    Ok(())
}

/// Map that stores pending responses for the outgoing requests (requests that
/// **we** send to the widget).
type PendingResponses = Arc<Mutex<HashMap<String, oneshot::Sender<ToWidgetAction>>>>;

pub(crate) struct WidgetProxy {
    info: WidgetInfo,
    sink: Sender<String>,
    pending: PendingResponses,
}

impl WidgetProxy {
    fn new(info: WidgetInfo, sink: Sender<String>, pending: PendingResponses) -> Self {
        Self { info, sink, pending }
    }

    async fn send<T: OutgoingRequest>(&self, msg: T) -> Result<OutgoingResponse<T::Response>> {
        let id = Uuid::new_v4().to_string();
        let message = {
            let header = Header::new(&id, &self.info.id);
            let action = Action::ToWidget(msg.into_action());
            to_json(&Message::new(header, action)).expect("Bug: can't serialise a message")
        };
        self.sink.send(message).await.map_err(|_| Error::WidgetDisconnected)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("Pending mutex poisoned").insert(id, tx);
        let reply = rx.await.map_err(|_| Error::WidgetDisconnected)?;
        T::extract_response(reply).ok_or(Error::custom("Widget sent invalid response"))
    }

    async fn reply(&self, response: IncomingResponse) -> StdResult<(), ()> {
        let message: Message = response.into();
        let json = to_json(&message).expect("Bug: can't serialise a message");
        self.sink.send(json).await.map_err(|_| ())
    }

    fn init_on_load(&self) -> bool {
        self.info.init_on_load
    }
}
