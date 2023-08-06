use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{from_str as from_json, to_string as to_json};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver as Receiver, UnboundedSender as Sender},
    oneshot,
};

use self::driver::Driver;
use super::{
    handler::{Driver as HandlerDriver, Incoming, MessageHandler, Request},
    messages::{
        from_widget::{FromWidgetMessage as FromWidgetAction, ReadEventResponse},
        openid::Request as OpenIDRequest,
        to_widget::ToWidgetMessage as ToWidgetAction,
        Message as WidgetMessage, Response as ResponseBody,
    },
    Error, Result,
};

mod driver;
#[macro_use]
mod process_macro;
mod widget;

pub type PendingResponses = Arc<Mutex<HashMap<String, oneshot::Sender<ToWidgetAction>>>>;

// Runs client widget API handler with a given widget. Returns once the widget is closed.
pub async fn run<T: widget::Api>(mut widget: widget::Widget<T>) -> Result<()> {
    // State: a map of outgoing requests that are waiting response from a widget.
    let state: PendingResponses = Arc::new(Mutex::new(HashMap::new()));

    // Spawn a ToWidgetMessage -> String sink. We use the typed sink in our code for safety.
    let (outgoing_req_tx, outgoing_req_rx) = unbounded_channel();
    tokio::spawn(forward(outgoing_req_rx, widget.comm.sink()));

    // Create a message handler (handles all incoming requests and generates outgoing requests).
    let driver = Driver::new(widget.info.id.clone(), widget.api, outgoing_req_tx, state.clone());
    let handler = MessageHandler::new(driver, widget.info.negotiate).await?;

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
async fn process_requests<T: HandlerDriver + Send>(
    mut requests: Receiver<FromWidgetAction>,
    responses: Sender<FromWidgetAction>,
    mut handler: MessageHandler<T>,
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

            SendToDevice (msg.request.clone())
                -> SendToDeviceRequest (resp),

            SendEvent (msg.request.clone())
                -> SendEvent (resp),

            ReadEvent (msg.request.clone().into())
                -> ReadEvents (ReadEventResponse { events: resp }),

            ReadRelations (msg.request.clone().into())
                -> ReadEvents (ReadEventResponse { events: resp }),
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
