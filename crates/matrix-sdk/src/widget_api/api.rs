use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{from_str as from_json, to_string as to_json};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver as Receiver, UnboundedSender as Sender},
    oneshot,
};
use uuid::Uuid;

use super::capabilities::Capabilities;
use super::messages::capabilities::Options;
use super::{
    handler::{Driver, Incoming, MessageHandler, OpenIDState, OutgoingMessage, Request},
    messages::{
        from_widget::{FromWidgetMessage as FromWidgetAction, ReadEventResponse},
        openid,
        to_widget::ToWidgetMessage as ToWidgetAction,
        Header, Message as WidgetMessage, Response as ResponseBody,
    },
    Error, Result,
};

macro_rules! handle_incoming {
    {
        chain = { $action:ident($msg:ident) -> $handler:ident -> $resp:ident },
        $($req_type:ident ($req_expr:expr) -> $incoming_type:ident ($resp_expr:expr) ),* ,
    } => {
        match $action {
            $(
                FromWidgetAction::$req_type(mut $msg) => {
                    if $msg.response.is_some() {
                        return Err(Error::InvalidJSON);
                    }

                    let (req, resp) = Request::new($req_expr);
                    $handler.handle(Incoming::$incoming_type(req)).await?;
                    let response = match resp.receiver.await.expect("Bug: handler died") {
                        Ok($resp) => ResponseBody::Response($resp_expr),
                        Err(err) => ResponseBody::error(err.to_string()),
                    };

                    $msg.response = Some(response);
                    FromWidgetAction::$req_type($msg)
                }
            )*
        }
    }
}

async fn forward(mut receiver: Receiver<impl Into<WidgetMessage>>, sink: Sender<String>) {
    while let Some(msg) = receiver.recv().await {
        let raw = to_json(&msg.into()).expect("Bug: invalid JSON");
        let _ = sink.send(raw);
    }
}

#[derive(Debug)]
pub struct WidgetInfo {
    pub id: String,
    pub negotiate: bool,
}

#[async_trait]
pub trait WidgetApi: Send + Sync + 'static {
    async fn initialise(&self, req: Options) -> Result<Capabilities>;
    async fn get_openid(&self, req: openid::Request) -> OpenIDState;
}

#[derive(Debug)]
pub struct Widget<T> {
    info: WidgetInfo,
    comm: WidgetComm,
    api: T,
}

#[derive(Debug)]
pub struct WidgetComm {
    from: Receiver<String>,
    to: Sender<String>,
}

struct WidgetDriver<T> {
    id: String,
    api: T,
    sink: Sender<ToWidgetAction>,
    pending: PendingResponses,
}

#[async_trait]
impl<T: WidgetApi> Driver for WidgetDriver<T> {
    async fn initialise(&self, req: Options) -> Result<Capabilities> {
        self.api.initialise(req).await
    }

    async fn get_openid(&self, req: openid::Request) -> OpenIDState {
        self.api.get_openid(req).await
    }

    async fn send<M: OutgoingMessage>(&self, msg: M) -> Result<M::Response> {
        let id = Uuid::new_v4();
        let raw =
            msg.into_message(Header { request_id: id.to_string(), widget_id: self.id.clone() });
        self.sink.send(raw).map_err(|_| Error::WidgetDied)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("Bug: poisoned mutex").insert(id.to_string(), tx);
        Ok(M::extract_response(rx.await.map_err(|_| Error::WidgetDied)?)?)
    }
}

pub async fn run<T: WidgetApi>(mut widget: Widget<T>) -> Result<()> {
    let pending: PendingResponses = Arc::new(Mutex::new(HashMap::new()));

    let (outgoing_req_tx, outgoing_req_rx) = unbounded_channel();
    tokio::spawn(forward(outgoing_req_rx, widget.comm.to.clone()));
    let driver = WidgetDriver {
        id: widget.info.id.clone(),
        sink: outgoing_req_tx,
        api: widget.api,
        pending: pending.clone(),
    };

    let handler = MessageHandler::new(driver, widget.info.negotiate).await?;

    let (req_tx, req_rx) = unbounded_channel();
    let (req_reply_tx, req_reply_rx): (Sender<FromWidgetAction>, Receiver<FromWidgetAction>) =
        unbounded_channel();
    tokio::spawn(process_requests(req_rx, req_reply_tx, handler));
    tokio::spawn(forward(req_reply_rx, widget.comm.to.clone()));

    let (resp_tx, resp_rx) = unbounded_channel();
    tokio::spawn(process_responses(resp_rx, pending.clone()));

    while let Some(raw) = widget.comm.from.recv().await {
        process_message(raw, req_tx.clone(), resp_tx.clone())?;
    }

    Ok(())
}

fn process_message(
    raw: String,
    req_sink: Sender<FromWidgetAction>,
    resp_sink: Sender<ToWidgetAction>,
) -> Result<()> {
    match from_json(&raw).map_err(|_| Error::InvalidJSON)? {
        WidgetMessage::ToWidget(msg) => {
            resp_sink.send(msg).map_err(|_| Error::WidgetDied)?;
        }
        WidgetMessage::FromWidget(msg) => {
            req_sink.send(msg).map_err(|_| Error::WidgetDied)?;
        }
    }

    Ok(())
}

type PendingResponses = Arc<Mutex<HashMap<String, oneshot::Sender<ToWidgetAction>>>>;

async fn process_responses(
    mut responses: Receiver<ToWidgetAction>,
    pending: PendingResponses,
) -> Result<()> {
    while let Some(to_widget) = responses.recv().await {
        let id = to_widget.id().to_string();
        let pending = pending.lock().unwrap().remove(&id).ok_or(Error::UnexpectedResponse)?;
        pending.send(to_widget).map_err(|_| Error::WidgetDied)?;
    }

    Ok(())
}

async fn process_requests<T: Driver + Send>(
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

            GetOpenId (openid::Request{ id: msg.header.request_id.clone() })
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
