use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{from_str as from_json, to_string as to_json};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver as Receiver, UnboundedSender as Sender},
    oneshot,
};
use uuid::Uuid;

use super::{
    handler::{Client, Incoming, MessageHandler, OutgoingMessage, Request, Widget as HandlerSink},
    messages::{
        from_widget::FromWidgetMessage as FromWidgetAction, openid::Request as OpenIDRequest,
        to_widget::ToWidgetMessage as ToWidgetAction, Header, Message as WidgetMessage,
        Response as ResponseBody,
    },
    Error, Result,
};

#[macro_use]
mod process_macro;
pub mod widget;

pub type PendingResponses = Arc<Mutex<HashMap<String, oneshot::Sender<ToWidgetAction>>>>;

/// Runs client widget API handler with a given widget. Returns once the widget is closed.
pub async fn run<T: Client>(client: T, mut widget: widget::Widget) -> Result<()> {
    // State: a map of outgoing requests that are waiting response from a widget.
    let state: PendingResponses = Arc::new(Mutex::new(HashMap::new()));

    // Spawn a ToWidgetMessage -> String sink. We use the typed sink in our code for safety.
    let (outgoing_req_tx, outgoing_req_rx) = unbounded_channel();
    tokio::spawn(forward(outgoing_req_rx, widget.comm.sink()));

    // Create a message handler (handles all incoming requests and generates outgoing requests).
    let handler = {
        let widget = WidgetSink::new(widget.info, outgoing_req_tx, state.clone());
        MessageHandler::new(client, widget).await?
    };

    // Spawn a task that receives requests from a widget, and passes them
    // to the handler, waits for response from a handler and sends it back.
    let (req_tx, req_rx) = unbounded_channel();
    let (req_reply_tx, req_reply_rx) = unbounded_channel();
    tokio::spawn(process_requests(req_rx, req_reply_tx, handler));
    tokio::spawn(forward(req_reply_rx, widget.comm.sink()));

    // Spawn a task that receives responses from a widget (for our outgoing requests),
    // matches them to the pending requests stored in a state (map) and sends them to
    // those who wait for them.
    let (resp_tx, resp_rx) = unbounded_channel();
    tokio::spawn(process_responses(resp_rx, state.clone()));

    // Receives plain JSON string messages from a widget and passes them
    // to the message processor that forwards them to the required handlers.
    while let Some(raw) = widget.comm.recv().await {
        process_message(raw, req_tx.clone(), resp_tx.clone())?;
    }

    Ok(())
}

/// Receives a plain JSON message, parses it and sends it to the corresponding processing task.
fn process_message(
    raw: String,
    req_sink: Sender<FromWidgetAction>,
    resp_sink: Sender<ToWidgetAction>,
) -> Result<()> {
    match from_json(&raw).map_err(|_| Error::InvalidJSON)? {
        WidgetMessage::ToWidget(msg) => resp_sink.send(msg).map_err(|_| Error::WidgetDied)?,
        WidgetMessage::FromWidget(msg) => req_sink.send(msg).map_err(|_| Error::WidgetDied)?,
    }

    Ok(())
}

/// Receives parsed requests from a widget, passes them to the handler, sends back the response.
async fn process_requests<C: Client, W: HandlerSink>(
    mut requests: Receiver<FromWidgetAction>,
    responses: Sender<FromWidgetAction>,
    mut handler: MessageHandler<C, W>,
) -> Result<()> {
    while let Some(raw) = requests.recv().await {
        let response = handle_incoming! {
            chain = { raw(msg) -> handler -> resp },

            GetSupportedApiVersion (msg.request)
                -> GetSupportedApiVersion (resp),

            ContentLoaded (msg.request)
                -> ContentLoaded (resp),

            GetOpenId (OpenIDRequest{ id: msg.header.request_id.clone() })
                -> GetOpenID (resp),

            SendEvent (msg.request.clone())
                -> SendEvent (resp),

            ReadEvent (msg.request.clone().into())
                -> ReadEvents (resp),

            ReadRelations (msg.request.clone().into())
                -> ReadEvents (resp),
        };

        responses.send(response).map_err(|_| Error::WidgetDied)?;
    }

    Ok(())
}

/// Receives parsed responses (to our outgoing requests) from the widget, matches them with the
/// pending outgoing requests and resolves them.
async fn process_responses(
    mut responses: Receiver<ToWidgetAction>,
    pending: PendingResponses,
) -> Result<()> {
    while let Some(to_widget) = responses.recv().await {
        let pending = pending
            .lock()
            .expect("Pending mutex poisoned")
            .remove(to_widget.id())
            .ok_or(Error::UnexpectedResponse)?;
        pending.send(to_widget).map_err(|_| Error::WidgetDied)?;
    }

    Ok(())
}

async fn forward(mut receiver: Receiver<impl Into<WidgetMessage>>, sink: Sender<String>) {
    while let Some(msg) = receiver.recv().await {
        let raw = to_json(&msg.into()).expect("Bug: invalid JSON");
        let _ = sink.send(raw);
    }
}

struct WidgetSink {
    info: widget::Info,
    sink: Sender<ToWidgetAction>,
    pending: PendingResponses,
}

impl WidgetSink {
    fn new(info: widget::Info, sink: Sender<ToWidgetAction>, pending: PendingResponses) -> Self {
        Self { info, sink, pending }
    }
}

#[async_trait]
impl HandlerSink for WidgetSink {
    async fn send<M: OutgoingMessage>(&self, msg: M) -> Result<M::Response> {
        let id = Uuid::new_v4();
        let request_id = id.to_string();
        let raw = msg.into_message(Header { request_id, widget_id: self.info.id.clone() });
        self.sink.send(raw).map_err(|_| Error::WidgetDied)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("Pending mutex poisoned").insert(id.to_string(), tx);
        Ok(M::extract_response(rx.await.map_err(|_| Error::WidgetDied)?)?)
    }

    fn init_on_load(&self) -> bool {
        self.info.init_on_load
    }
}
