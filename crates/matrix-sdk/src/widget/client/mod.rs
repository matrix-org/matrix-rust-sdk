//! Client widget API implementation.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use serde_json::{from_str as from_json, to_string as to_json};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use uuid::Uuid;

use self::handler::{
    Client, IncomingRequest, MessageHandler, Outgoing, Reply, Response, WidgetProxy,
};
pub use self::{
    handler::{Error, Result},
    matrix::Driver as MatrixDriver,
};
use super::{
    messages::{to_widget::Action as ToWidgetAction, Action, Header, Message},
    Info as WidgetInfo, Widget,
};

mod handler;
mod matrix;

/// Runs the client widget API handler for a given widget with a provided
/// `client`. Returns once the widget is disconnected or some terminal error
/// occurs.
pub async fn run<T: Client>(client: T, mut widget: Widget) -> Result<()> {
    // A map of outgoing requests that we are waiting a response for, i.e. a
    // requests that we sent to the widget and that we're expecting an answer
    // from.
    let state: PendingResponses = Arc::new(Mutex::new(HashMap::new()));

    // Create a message handler (handles incoming requests from the widget).
    let handler = {
        let widget = WidgetSink::new(widget.info, widget.comm.to, state.clone());
        MessageHandler::new(client, widget)
    };

    // Receives plain JSON string messages from a widget, parses it and passes it to
    // the handler.
    while let Some(raw) = widget.comm.from.recv().await {
        match from_json::<Message>(&raw) {
            Ok(msg) => match msg.action {
                // This is an incoming request from a widget.
                Action::FromWidget(a) => {
                    handler.handle(IncomingRequest { header: msg.header, action: a })?;
                }
                // This is a response from the widget to our request (i.e. response to the outgoing
                // request from our perspective).
                Action::ToWidget(a) => {
                    let pending = state
                        .lock()
                        .expect("Pending mutex poisoned")
                        .remove(&msg.header.request_id);

                    if let Some(tx) = pending {
                        // It's ok if it fails here, it just means that the handler died and the
                        // handler only dies once the widget disconnects.
                        let _ = tx.send(a);
                    }

                    // TODO: We don't seem to have an error response for the
                    // unexpected **responses** to our requests. Only for errors
                    // that occur during the processing of the **incoming
                    // requests**, not the **outgoing requests** (requests sent
                    // by us **to** the widget). Clarify that later.
                }
            },
            Err(..) => {
                // TODO: We need to construct a message to inform the widget
                // about the incorrectly formatted message, but it does not look
                // like we have a well-defined error mechanism for such cases.
                // Clarify that later.
            }
        }
    }

    Ok(())
}

/// Map that stores pending responses for the outgoing requests (requests that
/// **we** send to the widget).
type PendingResponses = Arc<Mutex<HashMap<String, oneshot::Sender<ToWidgetAction>>>>;

struct WidgetSink {
    info: WidgetInfo,
    sink: UnboundedSender<String>,
    pending: PendingResponses,
}

impl WidgetSink {
    fn new(info: WidgetInfo, sink: UnboundedSender<String>, pending: PendingResponses) -> Self {
        Self { info, sink, pending }
    }
}

#[async_trait]
impl WidgetProxy for WidgetSink {
    async fn send<T: Outgoing>(&self, msg: T) -> Result<Response<T::Response>> {
        let id = Uuid::new_v4().to_string();
        let message = {
            let header = Header::new(&id, &self.info.id);
            let action = Action::ToWidget(msg.into_action());
            to_json(&Message::new(header, action)).expect("Bug: can't serialise a message")
        };
        self.sink.send(message).map_err(|_| Error::WidgetDisconnected)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("Pending mutex poisoned").insert(id, tx);
        let reply = rx.await.map_err(|_| Error::WidgetDisconnected)?;
        Ok(T::extract_response(reply).ok_or(Error::custom("Widget sent invalid response"))?)
    }

    fn reply(&self, reply: Reply) -> Result<()> {
        let message = to_json(&Message::new(reply.header, Action::FromWidget(reply.action)))
            .expect("Bug: can't serialise a message");
        self.sink.send(message).map_err(|_| Error::WidgetDisconnected)
    }

    fn init_on_load(&self) -> bool {
        self.info.init_on_load
    }
}
